# A manual

**Status: detailed 2026-11-17; G.1 built 2026-11-17; G.2 built 2026-11-17.** Part G of [`pre-phase-7.md`](pre-phase-7.md): the system's
workings written down for the person using it and the programmer writing for it, read in the
terminal with `man` and a pager, from pages on the root filesystem whose source is in the
repository.

## Why

Nothing on the system explains the system. Almost nothing a person knows from Unix carries over:
pipelines carry tables rather than text, a command's options are often operators, and authority
comes from grants and `with` rather than `sudo`. The shell's syntax is where it hurts most
(the maintainer, 2026-11-17).

## Part G in detail

*Detailed 2026-11-17, measured on `main` at PR #450's merge (`6b4d7442`), the maintainer's calls
folded in.* **Seven sub-parts, a PR each.**

### What the pass found

1. **Nothing on the system explains the system.**
   - **The coreutils:** the 18 answer `--help` with 8 to 22 lines: a usage line, a sentence or two,
     the flags, often the output's table schema and the grant it needs. None has an example.
   - **The rest:** `nxsh --help` prints one usage line. `nxinstall` has a short help. The three
     applications and the 15 services have none.
   - **No help program:** there is no `man`, `help` or `apropos`, and no `help` builtin. Tab
     completion is the only way to find a command. The recovery shell has a `help` of its own.
   - **The desktop:** no Help menu. Its chords (`Super+A`, `Super+H`, `Super+R`, and `Super+1` to
     `Super+4` with and without Shift) are written nowhere a person can see.
2. **The shell language is specified for its implementers.**
   [`shell-language.md`](../spec/shell-language.md) is 1,640 lines. Its 26 code blocks state no
   output, and it is not on the system.
   - **§7, "an illustrative sketch", is stale:** it uses `print`, `ls` and `validate_schema`,
     which do not exist.
   - **One error message is stale:** `unavailable()` says a feature arrives "in Part D"
     (`userspace/nxsh/src/eval.rs`).
3. **The shell runs on the host.** `nxsh`'s library makes no syscalls, and its `Host` trait has a
   `MockHost` that answers a program with canned tables. `Interp::run_line` returns what the REPL
   would print. A test already runs a `.nx` file from the tools by `include_str!`. So an example
   using only the language and its operators can run in a host test. One that runs a `/bin`
   program needs a canned table, or a boot.
4. **The operators have no table.** `OPERATORS` is a flat list of 34 names. What each takes is
   spread over four parallel lists (predicate, brace-argument, nullary, terminal), and the dispatch
   is a string `match`. No operator has a signature or a description; what help there is lives in
   error messages.
5. **Every coreutil states its flags twice**: in its `Flag` table, with a line of help each, and
   again in its hand-written `HELP` text.
6. **No program knows its terminal's size** (`TODO(tty-size)`).
   - The tty ops, `0x0B00` to `0x0B0B`, carry no size. `nxterm` keeps its grid's size to itself.
   - `nxsh`'s completion listing assumes 80 columns.
   - On the serial console there is nothing to ask.
7. **A program's output reaches the screen only when its pipeline ends.** A stage's stdout is a
   typed stream the shell captures. To page, a program writes to its stage's terminal directly:
   - it writes with `OP_TTY_WRITE`, and sends `\r\n` itself, as nothing translates `\n`;
   - it reads raw keys with `OP_TTY_READ`, and the shell does not read the terminal while the
     pipeline runs;
   - Ctrl-C never arrives as data: the shell asks every stage to stop.
8. **`nxterm` has no alternate screen.** It draws cursor addressing, erasure, bold, underline,
   reverse and the 16 colours, and swallows every `?` sequence. A pager that quits leaves its last
   page on the screen.
9. **The shared key decoder drops Page Up and Page Down.** `libline`'s decoder knows the arrows,
   Home, End and Delete; `ESC[5~` and `ESC[6~` are dropped.
10. **A gate cannot read a redrawn screen in `nxterm`.** Its harness receipts report only the rows
    between the cursor's old and new rows, so a page drawn from the top reports one row.
    `test-interactive`'s serial transcript carries every byte, escapes included.
11. **The specs a programmer needs exist**, and could be pages as they are:
    - **The contracts:** [`syscall-abi.md`](../spec/syscall-abi.md) (663 lines, every syscall),
      the file formats (`init.toml`, service declarations, `theme.toml`, `views.toml`, desktop
      entries), the typed stream and the pipeline's stdio, and the protocols.
    - **What staging them takes:** of their 733 table rows, 256 are wider than 80 columns, and
      the widest is 2,414 characters. They hold 297 links into the repository, and a few HTML
      anchors and comments. A renderer for them must wrap a cell, fall back when a table cannot
      fit, show a link as its text, and drop HTML.
12. **`/system/manual` would not be reachable.** A session binds `/system/fonts` alone,
    read-only (`libsession/src/namespace.rs`).
13. **The services are declared one each**, as `[service.NAME]` with a description, in
    `/system/services.toml`, beside what `init` starts itself. A page about them can be checked
    against that list.
14. **The desktop's chords are registered one at a time** in `desktop-shell`'s `main.rs`, which is
    at its size entry (1,388).
15. **The shell's table display is the shell's alone.** `ops::display` and the value rendering it
    calls, about 150 lines, would page a table only from below the shell.
16. **Most files Part G would grow are at their size entry**:

    | Crate | Files at their entry |
    |---|---|
    | `nxsh` | `eval.rs` 4,137, `parse.rs` 1,867, `lex.rs` 1,089, `ops.rs` 1,079 |
    | `desktop-shell` | `main.rs` 1,388 |
    | `libterm` | `grid.rs` 1,600 |

    New code goes into new modules, and each sub-part says what moves first.

### The maintainer's calls (2026-11-17)

- **The shell first**, its syntax especially.
- **A general guide to the system and the services that start at boot**, for programmers to come.
- **The syscall interface documented.**
- **The desktop.**
- **Terminal only for now**: no window for the manual.
- **A pager.**
- **The shape** proposed the same day: pages are files in the repository, the shell guide's
  examples are run, and the sections are Unix's numbers.
- **The specs are staged as pages, as they are**, "to start": one copy, a spec reading acceptably
  at 80 columns, a table that cannot fit printing one record per row.
- **An alternate screen in `nxterm`**, so quitting the pager puts back what was on the screen, as
  `less` does: a second grid with no scrollback, switched by `ESC[?1049h` and `ESC[?1049l`.
- **Tables in the pager**, the shell's table display moved below the shell first (finding 15).
- **The written pages live in `docs/manual/<section>/`**, a new class of document that describes <!-- check-docs: allow-missing -->
  current behaviour, as `spec/` does.
- **`--help` is not the page.** It stays a short overview of a command's use and its options, as
  Unix's is; the page is the long form.
- **No `libman`.** The page renderer has one user, `man`, and the pager two, `man` and `page`, both
  coreutils: a helper with one consumer belongs to it, so both are modules of the coreutils
  library. The table display is the one piece two crates share, and moves into `libstream`.

### The page format

A page is Markdown, in the subset the specs already use:
- headings, paragraphs, bulleted and numbered lists one level deep, fenced code blocks;
- tables;
- inline code, bold and emphasis;
- links, shown as their text;
- HTML tags and comments, dropped.

**Its first line names it**: `# name — what it is in a few words`, the line `man` lists. Its
section is its directory's number:

| Section | Holds |
|---|---|
| 1 | Commands, and the applications |
| 2 | System calls |
| 5 | File formats |
| 7 | Overviews: the system, the shell, the desktop, boot, the protocols |
| 8 | Services |

**Rendered to a width**:
- paragraphs wrap, and headings are bold;
- code blocks are indented and not wrapped;
- a table's columns share the width, each cell wrapping within its column;
- a table whose columns cannot fit even narrowed prints one record per row, each cell under its
  column's name.

The renderer is `man`'s, in the coreutils library. The exact rules are a contract between the
pages, the renderer and the checks, written in
`docs/spec/manual-page.md` with G.1.

**Where pages come from**:
- `docs/manual/<section>/<name>.md`, the written pages; <!-- check-docs: allow-missing -->
- the specs staged as pages, chosen by a list in `xtask` that names each one's section and page
  name.

They are staged to `/system/manual/<section>/<name>.md` on every image, test, release and live,
and bound read-only into every session at `/system/manual`, as `/system/fonts` is.

### The order

- **G.1 first**: everything else is a page `man` shows.
- **G.2 needs nothing.**
- **G.3 needs G.1 and G.2**: the pager draws to the terminal's size.
- **G.4 needs G.1**, and **G.5 needs G.4**.
- **G.6 needs G.1**, and **G.7 needs G.6** for the check that spans `/bin`.

Built in that order, the shell's pages land fourth and fifth, after the pager that makes a page as
long as the shell guide usable.

### How each is held

- **Review and tests:** every sub-part gets a `/pr-review` and the host tests, and runs the KVM
  gates its files reach. Each new test fails without its change, which is its control.
- **Miri, once, at Part G's end**, over the job's targets Part G changes: `coreutils`, for the
  renderer and the pager; `libline`; `libstream`; `tty-server`; and `nxsh`.

Part G is done when G.7 has merged and what Miri reports is triaged.

### G.1 The page format, and `man`

- **The renderer**, a module of the coreutils library making no syscalls: it parses a page into
  blocks and renders them to a width, styled or plain.
- **`man`**, a coreutil:
  - **`man`** emits every page as a table, `Table<{name, section, summary}>`, so
    `man | filter section == 1` is a list of commands;
  - **`man NAME`** and **`man SECTION NAME`** print a page as text, at 80 columns until G.2;
  - **`man -k WORD`** lists the pages whose name or summary holds the word.
- **Staging**: `stage_rootfs` writes `/system/manual` from `docs/manual` and the list of staged
  specs. `libsession` binds it into every session, read-only.
- **Pages**:
  - `man(1)`;
  - `intro(7)` as a first page, saying what the manual will hold;
  - the staged specs, with the list.

**Tests**:
- the renderer: wrapping, a table's cells wrapping in their columns, the record fallback, links as
  text, HTML dropped, code left whole;
- **every page in `docs/manual` and every spec renders at 80 columns** with no line wider, but in
  a code block: a host test in the coreutils library reads them from the repository;
- `man`'s table, and `-k`.

**The gate**: `test-interactive` runs `man man` and `man | count` over serial.

**Docs**:
- [`documentation.md`](../conventions/documentation.md) and the root `CLAUDE.md` name `manual/`
  as current behaviour;
- `docs/spec/manual-page.md`.

**Built 2026-11-17** (`76e795ad`, `89a1853f`).
- **The renderer** is `coreutils::manual`, with `inline` for a line's markup; **`man`** is a coreutil,
  as planned.
- **The staging** is `tools/xtask/src/manual.rs`: `docs/manual`, and 31 specs by `STAGED_SPECS`,
  33 pages in all. Three specs are not pages, the kernel's own: the module ABI hash, the IRP
  layout and user-memory access. A test holds every spec to one list or the other.
- **`/system/manual` is a new namespace member, `bind_manual`**, set in both columns and every
  application.

**Departures, for the maintainer's eye:**
- **The manual is bound into applications too**, not only sessions: a terminal window's shell runs
  in its application's namespace, and would find no pages.
- **`man | filter section == 1`**, where the plan wrote `where`: the shell has no `where`.
- **The root `CLAUDE.md` names `docs/manual`** within its 200 lines: the status paragraph loses its
  list of the work before Phase 7, which had gone stale.
- **Two defects the shell spec found in the parser**, fixed with tests:
  - a line beginning `|` with no table under it sent the parser round a loop that never moved;
  - a table or a fence indented under a list item was read as the item's text.

**Twenty-two tests**:
- **The renderer, eighteen**: a heading's margin and its body's indent; wrapping, and a long word
  broken; lists, numbered and nested; code left whole; a table in columns, squeezed and as records;
  links, HTML and placeholders; styles; asterisks; pipes in cells; a summary; a `|` line with no
  table; a table and a fence under a list item; a name's page; a keyword; and **every page and
  spec at 80 columns**, each under a watchdog.
- **The staging, two**: every spec staged or said not to be, and every page in a section `man`
  reads.
- **The namespace's two**, extended: the manual bound where asked, and not where not.

**Controls**:
- fourteen mutations each fail a test, two of them after their tests were made to tell them
  apart: an asterisk inside a word, and a service before an overview;
- a spec left off both lists fails the staging test;
- `test-interactive` fails at `man man` with the serial session's bind removed.

**The gate**: `test-interactive` prints `man(1)` and `manual-page(5)`, and counts the 33 pages.

**After review** (#451, fifteen findings, every one fixed):
- **The pages were writable by every session**: the root filesystem's `/system/manual`, bound as a
  subtree of its one writable mount, took a `touch` from the serial session. **The maintainer
  chose the profile server**: the pages are the coreutils package's `manual/`, which its store
  hash covers, projected at `/system/manual` with base `/manual`, flat, as
  `<name>.<section>.md`. A session's `touch` and `remove` there are refused, from the serial <!-- check-docs: allow-missing -->
  session (the gate) and from a terminal window in a graphical one (a probe of `check-login`).
  **The fonts, bound the old way, are writable from a terminal window**, shown by the same probe,
  and left under `TODO(writable-fonts)`.
- **A page cannot drive the terminal**: control characters are dropped, a tab is spaces.
- **A spec's summary is its whole heading**, so `man --keyword protocol` finds the `rsproto-*`
  pages; **a page that cannot be read says so**, listed and asked for; **the listing's failure is
  said**; **a page goes out as `text_out` does, and a write failure is one**, a reader gone first
  being none.
- **Markdown it does not model is drawn as it is**, not dropped: asterisks that open nothing,
  `[text][ref]`, `![` with no link, text after a comment, `# Using C#`, extra cells, a code span
  of two backticks holding `|`, a fence of four. **A line breaks between words only**, never
  between a word's runs, as `(`a/b.rs`)` once did on the spec's first line.
- **`man`'s operands are the library's `request`**, its answer `answer`, both tested; the width
  is `TODO(tty-size)`'s.
- **Tests**: seven in the renderer, one in the stage, two in the staging; the namespace's
  extended, a failed fonts or manual bind non-fatal by a mock refusing a bind at one path; the
  section order pinned a pair at a time. **Controls**: a mutation for each fix fails its test.
- **The gate** tries `touch` and `remove` under `/system/manual`, recounts the pages, and finds
  one by keyword.

### G.2 The terminal's size

- **The tty protocol carries a size**:
  - `OP_TTY_SIZE` (`0x0B0C`) asks a terminal for its columns and rows, answering `0×0` for
    unknown, as on serial;
  - `OP_TTY_RESIZE` (`0x0B0D`) is the backend's: `nxterm` sends it on attaching and at every refit.
- **`libline`** decodes Page Up and Page Down, for every program that reads keys through it.
- **`nxsh`'s completion listing** fits the width. This resolves `TODO(tty-size)`, and with it the
  half of `TODO(history-pager)`'s condition that is the size.
- **`man`** renders to the width, or 80 when the size is unknown.

**Tests**: the size round trip through `tty-server`; a refit's resend; the decoder's two keys.

**The gate**: `check-terminal` asks the size after its grid's line and gets the grid it logged.

**Docs**:
- [`rsproto-tty-ops.md`](../spec/rsproto-tty-ops.md);
- `deferred-decisions.md`, for `tty-size`;
- [`console-and-tty.md`](../architecture/console-and-tty.md).

**Built 2026-11-17** (`26a000a9`, `105cb684`), with the editing the maintainer added to it.
- **The size**: `Tty::Size` and `Tty::Resize`, a backend's size in `tty-server`'s registry, the
  grid sent with `AttachBackend` and at every refit, and `librsproto::tty::Size`.
- **Asked by** `libprompt::size_on`, `Stage::terminal_size` and `nxsh`, which asks after every
  read; `man` renders to the width, and the completion listing fits it.
- **The editing**: `libline` draws by row and column where the width is known, from the prompt's
  width `nxsh` gives it, and a search's line goes through it.
- **`libline`** decodes Page Up and Page Down.

**Departures, for the maintainer's eye:**
- **`terminal`, a new coreutil**, printing the size, which the gate asks: the plan said
  `check-terminal` asks without saying how, and its grid is 80 wide, the fallback every program
  uses.
- **The size goes with the attach**, the plan's "on attaching" made a body rather than a message:
  the shell spawned next could ask before a separate one arrived.
- **Canonical reads still draw as a dumb terminal** (`TODO(canonical-wrap)`), and a serial line's
  size is unknown (`TODO(serial-size)`).
- **The discipline's tests moved** to `discipline/tests.rs` first, the file being at its cap.

**Tests**: the size's encoding; the registry's size per backend, through a resize and a re-point;
`size_on` and `terminal_size` against a far end, a refusal's four bytes included; the listing at a
width and the editor's; the prompt's width; a search drawn and accepted through the line; the page
keys; and **ten screens in `libterm`**, the discipline's output drawn in a grid: a wrap crossed by
Backspace and the cursor keys, a row filled exactly, an insert and a delete across rows, recalls
long to short, of several rows, over a long line and replaced, Enter mid-line, and a refit.
**Controls**: 27 mutations, each failing its test; with the width withheld from the discipline,
`check-terminal` fails at the wrapped row.

**The gates**:
- **`check-terminal`**: `terminal` answers the grid `nxterm` logged, and a line wider than the grid
  is edited across its wrap;
- **`check-login`**: after the drag, `terminal` in the session's window answers the new width;
- **`test-interactive`**: on serial, `terminal` says the size is unknown.

### G.3 The pager

**Moves first**: the shell's table display into `libstream`, if call 3 says so. Then the shell
and the pager print a table one way.

**`page`**, a coreutil, reads its stdin and pages it on its stage's terminal. The pager is a
module of the coreutils library, as `man` pages through it too:
- **Input**: text, or a table drawn as the shell draws it.
- **Keys**:

  | Keys | Move |
  |---|---|
  | Space, Page Down, `f` | A page down |
  | `b`, Page Up | A page up |
  | Down, `j`, Enter | A line down |
  | Up, `k` | A line up |
  | `g`, Home | The top |
  | `G`, End | The end |
  | `/word` | The next line holding the word; `n` repeats |
  | `q` | Quit |

- **The bottom row says where it is.**
- **Input that fits on one screen prints without paging.** With no terminal, as in a script, the
  input passes through unchanged.
- **`man` pages through the same library** when it has a terminal.

**The alternate screen**, if call 2 says so: `libterm` keeps a second grid, with no scrollback,
switched by `?1049h` and `?1049l`.

**`nxterm`'s harness receipts** report every row a chunk changed, so a gate can read a redrawn
page.

**Tests**:
- the pager's model on the host: the lines, the top row, a key's effect, a search;
- the alternate screen's switch and restore;
- the receipts covering a full redraw.

**The gates**:
- **`check-terminal`**: `man` on a long page, then Space and the next page's rows, then `q` and
  the prompt again.
- **`test-interactive`**: over serial, the pager at 80 by 24.

**Docs**: [`pipeline-stdio.md`](../spec/pipeline-stdio.md), on a stage drawing to its terminal;
`page(1)`.

### G.4 The shell's operators, and `help`

**Moves first**: the operator lists out of `parse.rs`, into a new `nxsh/src/operators.rs`. It
holds one entry per operator:
- its name and its kinds (predicate, brace argument, nullary, terminal);
- its signature;
- a line saying what it does;
- an example and its output.

The four parallel lists become questions asked of the table.

- **`help`**, a builtin:
  - `help` alone lists the operators by group, and the builtins and keywords, with a pointer to
    `man shell`;
  - `help NAME` prints an operator's, a builtin's or a keyword's entry, and for anything else
    names the `man` page to read. The shell renders no page itself.
- **`shell-operators(7)` is checked against the table**: a host test renders the table to the page
  and compares, so an operator added without its entry, or a page that drifts, fails.
- **Every example in the table runs on the host**, and its output is compared.
- **The stale pieces**: §7 of the shell spec goes, and `unavailable()`'s message is fixed.

**Tests**:
- every name the dispatch knows has an entry, and no entry lacks a name;
- the examples;
- `help`'s two forms.

**The gate**: `test-interactive` runs `help filter` over serial.

**Docs**: [`shell-language.md`](../spec/shell-language.md), whose §10b points at the table.

### G.5 The shell guide

**`shell(7)`, a tour in a person's register.** Each example is a fenced `nxsh` block followed by
its output. It covers:
- running a command, and a pipeline of typed streams;
- tables: `filter`, `select`, `sort`, `take`;
- values and their types, strings and quoting, variables;
- blocks and control flow;
- errors and `try`;
- `def` and scripts;
- the prompt's keys and history;
- where `with` and grants come in.

**Its examples are run.** A host test runs every one through `Interp`, with a `MockHost` whose
programs answer canned tables. A guide the language stops matching fails CI.

**The spec stays the contract**, and the guide points into it for the exact rules.

**The gate**: `test-interactive` runs one of the guide's examples that calls a real program.

**Docs**: `shell(7)`; the root `README.md` points a reader at `man intro`.

### G.6 The commands

- **A page in section 1 for every program a person runs**:
  - the 18 coreutils, `man` and `page`;
  - `nxsh` and `nxinstall`;
  - `nxterm`, `nxfiles` and `nxedit`: what the window does, its menus and its keys.
- **`--help` stays a short overview**, Unix's: a usage line, a sentence, and the options. **The
  options are printed from the program's `Flag` table**, whose lines each already carry, so a
  flag is described once where it is parsed. The `HELP` texts keep the usage line and the
  sentence. Every program in section 1 answers `--help`, the applications and `nxsh` included.
- **The page is the long form**: what the command does, its output's schema, the grant it needs,
  its errors, examples. **A test holds every flag a program parses to its page**, by name, so a
  flag added without its paragraph fails.
- **The check across `/bin`**: every program in the image's `/bin` has a page in section 1, or is a
  service on the services page.

**The gate**: `test-interactive` runs `list --help` and `man list`.

Author; the pages can be the implementer's, from the `HELP` texts and the code.

### G.7 The system, the desktop, and a programmer's pages

**Moves first**: the desktop's chords into one table in `desktop-shell`, registered from it.

- **`intro(7)`, start here**: what the system is and how it differs (capabilities, namespaces, no
  root, grants and `with`, typed pipelines), and where to read next.
- **`boot(7)` and `services(8)`**:
  - `boot(7)`: what starts, in order, and who supervises what: `init` and its mounts,
    `service-mgr` and each service, the sessions, the desktop;
  - `services(8)`: a section per service, saying what it serves and where, its restart policy, and
    how to see, stop and start it.
  - **A check holds the page to the image**: every `[service.NAME]` declared, and every program
    `init` starts, has a section.
- **`desktop(7)`**: the panel, windows, desktops, the overview, the menus, and every chord. A test
  holds the page to the chord table.
- **A programmer's pages are the staged specs**:
  - `syscalls(2)` is [`syscall-abi.md`](../spec/syscall-abi.md);
  - the file formats are section 5;
  - the typed stream, the pipeline's stdio and the protocols are section 7.

**The gate**: `test-interactive` runs `man syscalls` and `man desktop`.

### What Part G does not do

- **A window for the manual**: terminal only, the maintainer's call.
- **The shell's history as a list** (`TODO(history-pager)`). G.2 gives it the terminal's size;
  the list is the shell's, later.
- **The specs rewritten for a person.** They are staged as they are, and the written pages point
  into them.
- **The kernel's internals as pages**: the IRP layout, the ABI hash and user-memory access stay in
  the repository.
- **Rust API documentation**, which is `cargo doc`'s.
