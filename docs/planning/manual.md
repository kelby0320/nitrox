# A manual

**Status: detailed 2026-11-17; G.1 built 2026-11-17; G.2 built 2026-11-17; G.3 built 2026-11-17; G.4 built 2026-11-18; G.5 built 2026-11-18; G.6 built 2026-11-18.** Part G of [`pre-phase-7.md`](pre-phase-7.md): the system's
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
  uses. **Shaped to grow**, the maintainer's call: one command, its queries the record, null where
  unknown, and its settings options that ask the window (`shell-language.md` § 10).
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
- **`test-interactive`**: on serial, `terminal` reports the size null.

**After review** (#452, thirteen findings: nine fixed, three deferred, one declined; `8f1dc751`,
`f979fe85`):
- **A refit moved the cursor off the line**: `libterm`'s reflow dropped every trailing blank and
  held the cursor to its line's text, so a space typed at the prompt, or a line the refit filled
  exactly, left the cursor a cell or a row from where the shell counted it, and what ran was not
  what was shown. **A row now keeps what was printed in it, blanks too, and the cursor its
  offset**, its line given the row to reach it. The discipline counts a recalled statement's
  earlier rows as their text fills them, ends a row its text fills exactly with a line feed rather
  than leave it wrapped into the next, and reaches the end of a row a refit filled by writing its
  last character again, as typing reached it.
- **Tab's listing goes below the line's end** (`Discipline::leave`, Enter's move), not from the
  cursor's row over a wrapped line's later rows; **`Ctrl-C` passes the line at the width the
  terminal has now**, the shell asking then too.
- **A size is read from a body's first four bytes**, any after them a later version's, as the wire
  format's schema evolution has it; `Size::decode` had wanted exactly four where `nxsh` read the
  first four. `OP_TTY_ATTACH_BACKEND`'s doc says its body is the grid.
- **Deferred**: a line taller than its window (`TODO(tall-line)`), a `Ctrl-C` lost between a size
  asked and a prompt (`TODO(size-then-prompt)`), and a key reaching the server before a refit's
  size (`TODO(resize-unsent)`). **Declined**: pinning `\r` as the move to column 0, which no
  terminal here tells from `CSI D`.
- **Tests**: nine screens in `libterm` and three in its grid, one each in `libline` and the size,
  two in the editor, and **a randomized one**: edits, recalls, refits of width and height, at the
  screen's top and its foot, 240 runs checked after every step against the text laid out afresh.
  The boot probe sends `AttachBackend` a body too short to be a size. **Controls**: 21 mutations,
  each failing a test; the randomized check at 20,000 runs found 533 failures and then 21 before
  the last two pieces, and none after; `test-qemu` fails with the size's refusal removed.

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

**Built 2026-11-17** (`aa8625c9`, `2a643748`, `b365dfb1`, `7571317f`, `7b7c4a2f`, `a7d99dc7`).
- **The table display** moved into `libstream::display`, with the value rendering under it and the
  header's colour: the shell and the pager lay a table out, and paint its header, one way.
- **The alternate screen** in `libterm`: `ESC [ ? 1049 h` and `l`, a second grid with no history,
  the main one rewrapped beneath it at a refit.
- **The pager**, `coreutils::pager`: a pure model, the keys the plan named, a status row; and a
  driver that pages on the stage's terminal, echo off, the keys read raw, the size asked at every
  key, `Ctrl-C` leaving as `q` does. **`page`**, and **`man`** through it, a page or its listing.
- **`nxterm`'s receipts** report every row a chunk changed while the alternate screen is up.

**Departures, for the maintainer's eye:**
- **A stage pages only where its output is shown**, the maintainer's call: the plan's "when it has
  a terminal" would page inside `man | count` and `let x = man ls`, a stage having a terminal
  mid-pipeline. The setup message gains `shown`, appended, which the shell sets on the last stage
  of a pipeline, a program's call or a bare name the REPL displays, and nowhere else
  ([`pipeline-stdio.md`](../spec/pipeline-stdio.md) § Payload). Where `page` is not shown, or one
  page holds its input, it passes the input on unchanged; `man` writes as ever.
- **`check-terminal` pages `page`'s input, not `man`**: the test image's `nxterm` is a service in
  init's namespace, which holds no manual. `test-interactive` pages `man`.
- **The receipts cover the alternate screen only**: on the main one, output scrolls and every row
  would change, a flood the gates' scans would trip on.
- **`libline::Keys`**, the discipline's escape decoding on its own, so the pager reads keys as the
  shell does; and `libprompt::write_on` and `set_echo_on`.
- **Size raised by hand**: `eval.rs` by seven lines, where the statement sets the flag and its
  pipeline takes it, and the view broker's `main.rs` by one, its literal's field.

**Tests**: the display's layout and rendering, moved with it, and the shell's paint of it; the
decoder alone; the alternate screen's switch, its history, its refit, and its parsing; `shown`
through the payload, the send and the receive, which run is told line by line, and only the last
stage; the pager's rows, controls, keys, search, status, refit and drawing, and on a terminal the
mock plays, its alternate screen, echo, a refit at the next key, `Ctrl-C`, a failing terminal and
a page that fits; `write_on` in parts, and echo. **Controls**: 9 mutations of the alternate screen,
11 of `shown`, 20 of the pager, each failing a test; `check-terminal` fails at the second page's
last row with the receipts' alternate-screen rows off, and at the first page's status with `shown`
never set.

**The gates**:
- **`check-terminal`**: `1..300 | map { |n| {n: n} } | page`, paged a row short of the grid, turned
  with Space, its second page's last row read, left with `q` for the prompt;
- **`test-interactive`**: `man man` paged at 80 by 24 and turned, `man 5 manual-page` paged and
  left, and a table far over a page passed through `page` to `count` (57 steps).

**After review** (#453, fourteen findings: nine fixed, two deferred, one the maintainer's call
written down, one declined, one refuted; `b1260430`, `47315438`):
- **`n` goes on from the page shown**: from the line last found only while the page shows it, so
  moving back above a match no longer skips every match before it.
- **A raw read takes a count** (`rsproto-tty-ops.md` § Read): the pager reads a byte at a time, so
  what is typed after `q` stays queued for the shell. **It turns no echo off**: the server echoes a
  canonical read only, which a routing test now holds; `libprompt::set_echo_on` went.
- **An escape sequence is dropped whole**, so a coloured cell shows its text, not `[31m`.
- **What a program paged is not kept**, the maintainer's call, written into the shell spec's
  `$last` and both pages.
- **Deferred**: a diagnostic the shell draws while a page is up (`TODO(diag-on-alternate)`), and a
  stage that ends on the alternate screen (`TODO(alt-screen-crash)`). **Declined**: a host test for
  the shell's own `send_setup`, in its binary, which `check-terminal` holds. **Refuted**: the gates'
  paging waits, each failing where a broken Space should fail them.
- **Tests**: the terminate request and `show`'s guard driven through a stage built from a setup
  message; a refit's clamp and an empty search; the alternate screen's selection, damage, crop and
  cursor, against a grid's invariants; the raw read's count, its rest, and its echo.

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

**Built 2026-11-18** (`842f3b48`, `dfface44`).
- **The table**, `nxsh/src/operators.rs`: the 34 operators, each with its family, how its arguments
  read, whether it ends a chain, its signature, a line, and an example with what it prints. The
  parser and the REPL ask it.
- **`help`**, a builtin: the operators by family, the builtins and the keywords; `help NAME`, an
  entry; a program's name, its page. The builtins and keywords have entries in `nxsh/src/help.rs`.
- **`shell-operators(7)`**, `docs/manual/7`, the table as a page.
- **The stale pieces**: §7 of the shell spec is a tombstone, and a builtin after a pipe is told that
  nothing is piped into it.

**Departures, for the maintainer's eye:**
- **`help` points at `man shell-language`**, not `man shell`: `shell(7)` is G.5's, which moves the
  pointer when the page exists.
- **`help` returns its text as a value**, so it may head a pipeline (`help trim | count`); `cd` and
  `exit` may not.
- **The builtins and keywords have entries**, with examples run where one can, beside the operators.
- **§3's table said `expect`, `assert` and `parse` were operators**; they are keywords, and it says so.

**Tests**: every example in all three tables, run; the dispatch's arms against the table; the
twelve operators that take nothing named and parsed; the keywords against the lexer's; names once,
ASCII, within 80 columns; `help`'s two forms, a program and an error; a builtin heading a pipe and
refused one; the page against the table. **Controls**: 14 mutations of the table, the dispatch and
`help`, each failing a test; `test-interactive` fails at `help filter`'s entry with `help` no
builtin, and at its example's output with the output left out.

**The gate**: `test-interactive` runs `help filter` and reads its entry, its example's output among
it, over serial (58 steps).

**After review** (#454, nine findings, every one fixed; `ef879ba2`):
- **Only `help` heads a pipeline.** Every builtin could, so `cd / | count` moved and then failed,
  and `exit 3 | count` left the shell. Each builtin now says whether it may (`builtins.rs`, which
  the parser asks for the builtins, where it asked `help.rs`); `cd` and `exit` at a pipe's head are
  refused before they act.
- **`help PROGRAM` names a page the manual has**, and the program's `--help` where it has none,
  which is every program but `man` and `page` until G.6; `/bin` is listed only once no table has
  the name. A function or a name bound here is said to be one.
- **A builtin's name at a statement's head is the builtin**, as `cd`'s always was: §3 says so, and
  a binding named `help` is read alone or in parentheses.
- **The smaller pieces**: `nxsh`'s `CLAUDE.md` names the tables; two group headings said what
  their operators do not; the builtins' docs said they mutate the shell; `help`'s arms the parser
  never reaches went; `exit`'s comment is back above it; and the run that rewrites the page passes.

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

**`help` points at it**: since G.4 its listing and its keywords' entries name `man shell-language`,
for want of `shell(7)`.

**The gate**: `test-interactive` runs one of the guide's examples that calls a real program.

**Docs**: `shell(7)`; the root `README.md` points a reader at `man intro`.

**Built 2026-11-18** (`18e28bee`).
- **`shell(7)`**, `docs/manual/7/shell.md`: a command, a pipeline, tables, values and their types,
  strings, variables, blocks, errors and `try`, functions and scripts, the prompt's keys, and
  `with`. 20 examples, each typed after `>` and followed by what the shell prints.
- **Its examples are run** (`nxsh/src/guide_tests.rs`): typed at one prompt in order, as one
  session would type them, on a mock machine whose `/bin` lists six programs and whose `whoami`
  is `alice`; a multi-line one joined as the REPL joins it. A `text` block headed `# /PATH` is a
  file on that machine, which the `use` example reads.
- **`help` points at it**: `help`'s listing names `man shell` first. Its entries keep
  `man shell-language` for the rules, and `shell-operators(7)` points at both.
- **Where to start**: `intro(7)` lists the shell's three pages, and the root `README.md` points at
  `man intro` and `man shell`.

**Departures, for the maintainer's eye:**
- **An example's lines start with `> `**, inside the `nxsh` block: drawn by `man`, a block and its
  output are alike, and the mark tells what to type from what comes back. The test strips it, and
  refuses a line without it.
- **Its examples share one session**, so one may use what an earlier one bound, as a reader typing
  along would.
- **A program's answer is one machine's**: the mock's sizes, which the page says will differ.
  `list /bin | filter name == "man" | select name kind`, which the gate types, answers the same on
  any machine.

**Tests**: every example, its output against the page's; the page read whole, an output after
prose and a line with no `> ` refused; the gate's example against the guide's. **Controls**: an
output changed, an output after prose taken as one, lines typed one at a time, the script file
not made, and `help` pointing elsewhere each fail a test; `test-interactive` fails at the
example's row with `select` keeping one column.

**The gate**: `test-interactive` types the guide's `list` example over serial and reads its row
(59 steps).

**After review** (#455, ten findings: nine fixed, one declined; `33171cb2`):
- **The guide said what the shell does not**: `for` walks no string; `let` may bind a name again,
  where `const` may not, and neither is fixed when a script is read; `$last.status` could be an
  earlier line's. Each claim is now an example the test runs, or is said as the source does it.
- **Two shell faults it described, fixed**: a line that ran no pipeline paired its value with an
  earlier line's status, and now has none; and a program that failed and sent nothing let the
  operator after it blame its missing operand, where now the pipeline fails naming the program,
  its stages in the `catch`. The spec's §1 and §11d say so. `eval.rs`'s entry was raised by hand,
  seven lines.
- **The guide's test sees what it could miss**: a fence indented under a list item or of tildes
  is refused, and what a line writes to `stderr` is shown before its value.
- **The gate reads the example's whole answer**, its header before its row.
- **Declined**: comparing the mock's `/bin` with the real `list`'s, which would make the shell's
  tests depend on the coreutils; the gate runs the one example whose answer is the same anywhere.
- **CI's `check-copy` flaked again**, a log line landing inside the copy's row before its count
  (after it, the last time). The maintainer's call: fixed here (`042edb98`). The gate finds the
  count as a whole number between the copy's line and the next prompt, wherever a line lands.

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

**Built 2026-11-18** (`198fc6fe`, `27bab753`).
- **A page in section 1 for every program in `/bin`**: the 21 coreutils, `nxsh`, `nxinstall`, and
  the applications `nxterm`, `nxfiles` and `nxedit`, with their menus and keys. 24 new pages, the
  implementer's from the sources and the spec's rows, each claim checked against the source; a
  test holds that every program `/bin` ships has one.
- **A coreutil's flags and usage are one table**, `coreutils::usage`: the program parses with it,
  and `--help` prints the usage line, a sentence, and the options from it, lined up and wrapped at
  80 columns. A flag carries the words after it (`--mount DEVICE [LABEL]`).
- **Every program in section 1 answers `--help`**: `nxsh` with a short overview, and the three
  applications, before they open a window (`libstream::channel::answer_help`).

**Departures, for the maintainer's eye:**
- **The tables are in one module of the coreutils library**, not each program's file: one test
  can then hold every program's flags to its page, and its `--help` to 80 columns.
- **`with` keeps reading its options by hand**, the first word deciding; its table is held to the
  arms that read them, both ways.
- **A flag is held to its page's Options section**, not the page at large, where a passing mention
  would satisfy it.
- **No service is in `/bin`**, so the check has nothing to send to the services' page yet.
- **Size raised by hand**: `nxfiles`'s `main.rs` by one line, its `--help`.

**Tests**: every program's `--help` within 80 columns and naming each flag; the layout of `list`'s
and `disk`'s; every flag in its page's Options; every flag a program asks for in its table;
`with`'s table and its arms; every program in `/bin` with a page. **Controls**: a flag dropped from
its page's Options, a program asking for a flag its table lacks, a flag dropped from `with`'s table,
`--help` wrapped at 100, and a page removed, each fail a test; `test-interactive` fails at
`nxedit --help` with `--help` unanswered.

**The gate**: `test-interactive` runs `list --help`, `man list`, paged and left, and
`nxedit --help` (62 steps).

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
