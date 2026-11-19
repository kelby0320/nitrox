# intro — the system, and where to start

This is Nitrox's manual. `man` lists its pages, and `man NAME` prints one; `man man` says how. This
page says what kind of system it describes, and in what order to read.

## What the system is

Nitrox does not work as a Unix does, and a habit from one will mislead in a few places. The
differences are the system's design, not gaps in it.

- **Authority is a handle a program holds, not who it runs as.** The kernel has no user, no group
  and no login. A program can do what the handles it was given let it do, and nothing else. An
  account exists only above the kernel, to decide what a session is handed.
- **A program sees what it was given.** Each program has its own namespace, the set of paths it
  can name. Two programs at the same path can see different things, and a path that was never
  bound is not refused: it is not there. A session is sandboxed by what it was handed, and `/dev`
  holds only what its owner chose to put in it.
- **There is no root and no superuser.** Nothing runs as an all-powerful account. A session starts
  with the authority its account was given and no more; a command that administers the machine is
  run with more, one command at a time, in a view that grants it: `with admin service --restart
  NAME`. The system's policy says which accounts may ask for which views, and whether a password
  is wanted. See `with(1)` and `views.toml(5)`.
- **Programs pass tables, not text.** A pipeline sends typed values, most often a table with named
  columns, from one stage to the next, so `list /bin | filter size > 500000 | sort size` needs no
  parsing. The shell is `nxsh`; see `shell(7)`.
- **There are no signals.** A program learns of an event, such as a child's exit, from its
  notification queue, which it reads when it chooses. A stop is a request a program may or may not
  honour; `service(1)` says so when a service does not.

## Sections

| Section | Holds |
|---|---|
| 1 | Commands |
| 2 | System calls |
| 5 | File formats |
| 7 | Overviews: the system, the shell, the protocols |
| 8 | Services |

## Where to read next

- `shell(7)`, first: a tour of the shell you type at, with every example one you can type back.
  `shell-operators(7)` has each operator with an example, and `shell-language(7)` the rules.
- `man(1)`, which reads this manual, and `page(1)`, which shows a long output a page at a time.
  `man --keyword WORD` finds a page by a word in its name or its summary.
- `boot(7)`, what starts when the machine is turned on, in what order, and who supervises what.
- `services(8)`, each service the system runs, what it serves and what breaks without it; and
  `service(1)`, which shows and controls them.
- `desktop(7)`, the graphical session: logging in, the desktop and its applications.
- The programmer's pages, which describe the system as it is:
  - in section 2, what a program calls: `syscalls(2)`, then `handles(2)`, `io(2)` and the rest;
  - in section 5, what a program reads and writes: `init.toml(5)`, `services.toml(5)`,
    `theme.toml(5)`, `views.toml(5)`, `typed-stream(5)` and the rest;
  - in section 7, how the parts speak: `pipeline-stdio(7)`, `device-nodes(7)`, and the
    resource-server protocols, `rsproto(7)` and the pages named after it.
