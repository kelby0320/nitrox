# Documentation conventions

How the docs under `docs/` are kept true. The directories are listed, and the short form of these
rules given, in the root `CLAUDE.md`, *Repository layout*.

**Which of these describe the system as it is today**, and which do not — this matters more
than it looks, because reading the wrong class as current is how you end up confidently
wrong about how something works:

- **`spec/`, `reference/`, `architecture/`, `conventions/` describe current behaviour.**
  If one disagrees with the source, the source wins and the doc is a bug — fix it in the
  same change.
- **`rationale/` explains why**, and is largely timeless.
- **`design/`, `planning/` and `archive/` do not describe current behaviour.** `design/`
  is what a subsystem *will* be. Today it holds `fault-survival.md`
  (added 2026-08-19), which is not a display document at all — it is where the kernel's
  fault-survival intent is written down — `graphical-prompt.md` (added 2026-09-29), the password
  prompt the view broker will open on the display once something needs one, and `nitrox-shell/`,
  the designed-but-not-built appearance of the desktop, which `docs/planning/desktop-refresh.md`
  adopts. What is built has moved out —
  `input-subsystem.md` and `widget-toolkit.md` graduated on 2026-08-12, `desktop-shell.md` and
  `graphical-session.md` on 2026-08-25 with Milestone 7, `ui-composition-model.md` on
  2026-08-26 with Milestone 8, and `display-substrate.md` on 2026-08-30 (owed by Milestone 9 and
  paid a milestone late) — so the rule to apply is simply "`design/` means not built".
  **Two docs graduated while still outrunning their code** — `desktop-shell.md` (its tray is
  v2) and `ui-composition-model.md` (its ports are unscheduled) — and each says in its Status
  line which sections are behaviour and which are intent. That is the pattern to copy when a
  document is mostly true, rather than one to spread.
  `planning/` is what is intended, with checkboxes for what is done. `archive/` is superseded.
  **Never conclude "the system does X" from any of them.**
- **`decision-log.md` is a dated record and is append-only.** Entries are true as of their
  date; correcting one to match today's code destroys the evidence. Append a new entry.

**A `design/` doc graduates to `architecture/` when the code lands** — the milestone that
builds it should carry that move as a checkbox, because the doc is otherwise the thing
nobody remembers to update.

**Filenames carry no version number.** `foo-design-v1.2.md` guarantees link rot on every <!-- check-docs: allow-missing -->
revision — renaming it breaks every inbound reference, which is a self-inflicted source of
exactly the drift these rules exist to prevent. The version belongs in the doc's Status
line; git carries the history. (`archive/os-design-v5.1.md` is the exception: there the
version is the artifact's identity and the file is frozen.)

Every doc under `architecture/` carries a **Status** line naming what is actually built and
when it was last checked. Trust it over the body's tense, and correct it when you find it
wrong.

`cargo xtask check-docs` (in CI) enforces the mechanical part:

- every relative doc link resolves, and every heading a link names exists;
- every doc cited in prose, as a backticked `docs/…` path or a bare `*.md` name, exists;
- every backticked `kernel/…`/`userspace/…`/`tools/…` `.rs` or `.toml` path cited by a
  current-behaviour doc exists;
- every `architecture/` and `design/` doc has a Status line in its first 20 lines;
- the syscall table in `docs/spec/syscall-abi.md` and the error codes in
  `docs/reference/error-codes.md` agree with the kernel's;
- the root `CLAUDE.md` is at most 200 lines, and its index names every other `CLAUDE.md` git
  lists, and nothing else.

It cannot tell whether prose is *true* — that part is on review. A deliberate reference to a path
that does not exist (an honest forward reference, or a record of a deletion) is exempted by
marking the line `<!-- check-docs: allow-missing -->`.
