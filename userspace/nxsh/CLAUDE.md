# userspace/nxsh/CLAUDE.md

`nxsh` — the Nitrox shell. Loaded when Claude Code reads files under `userspace/nxsh/`.

## What this is

The shell and its language: lexer, parser, evaluator, generic value operators, regex, and
the interactive loop. Scripts are `.nx`.

**It is the login leaf** as of 2026-07-31 — `session-mgr` spawns it into the constructed
session namespace with empty syscaps. It replaced the throwaway `usersh`, which is gone.

- Design (semantics/grammar): `docs/history/nitrox-shell-design-v1.1.md`
- Build plan and the decisions it resolved: `docs/planning/shell-coreutils-plan.md`

## Structure — and the reason for it

- **`src/lib.rs` and friends — the language.** `lex`, `parse`, `ast`, `eval`, `value`,
  `ops`, `repl`, `regex`. **No syscalls.** Host-tested (`cargo test -p nxsh --lib`) in a
  second rather than through a 90-second boot.
- **`src/main.rs` — the host.** `_start`, spawning stages, wiring pipes, the console
  reader, the filesystem.

The seam is the **`Host` trait**: everything that touches the OS sits behind it, the same
way the ext4 parser sits behind `BlockReader`. It is worth more here than there — an
interpreter is mostly pure logic, and the parts that are not (pipeline ordering, per-stage
status, a crashed stage, the `strict` abort) are exactly the ones hardest to provoke on
real hardware. `MockHost` makes all of them ordinary tests.

**Do not reach for a syscall from the library half.** If the evaluator needs something from
the OS, it goes on the `Host` trait — otherwise the test suite quietly stops covering it.

## Build environment

`#![cfg_attr(not(test), no_std)]` + `alloc`. The library host-tests on the host target; the
bin builds for `x86_64-unknown-nitrox`. No nightly features.

## What the language is *not*

- Not `key=value` env: the environment is a TSM1 `Record`, typed, arriving on the setup
  message (Milestone 3.5). `PATH` is a `List<String>`.
- Not truthy: a non-`Bool` condition is an error (§6's fail-loud rule).
- Not wrapping: arithmetic overflow and division by zero are errors, not fabricated values.

## Things that bit, and should not bite twice

- **`is_ident_start` and `is_ident_char` are different sets.** `$` starts an identifier
  (`$env`, `$last`) and does not continue one. `scan_ident` must begin *past* the first
  byte, or a start-only character consumes nothing and the scanner spins — an infinite
  loop, not a wrong token.
- **A bareword argument means different things per operator.** `sort size` names a column;
  `display files` names a binding. Arguments are kept unevaluated and each operator
  decides, because only the operator knows.
- **A builtin's argument is a path, not an expression.** `cd ..` and `cd /system` both
  choke in expression mode, where `..` is a range and `/system` a division.
- **The interactive loop in `main.rs` may intercept exactly one line: `exit`.** It must end
  the loop, which `run_line` cannot do. A `cd` guard sat beside it, left from before `cd`
  existed, and went on refusing a builtin the interpreter had implemented — for weeks,
  because the script path calls `run_line` and the interactive path never got there. Any
  new special case here is a second implementation of the language.
- **`Host::exists("/")` is true by construction.** A namespace root has nothing bound *at*
  it and no server owning it, so both of `exists`'s probes miss — yet `list /` enumerates
  it. `cd /` and `cd ..` out of `/home` were both refused until this was special-cased.
- **The shell does not rewrite a spawned stage's paths.** It passes `argv` through as
  written and hands over the same `PWD`, so both sides resolve identically. Pre-resolving
  would reintroduce the split-brain the environment design exists to prevent.

## Forbidden patterns

- Syscalls in the library half (use `Host`).
- A second path-resolution implementation — `librsproto::path::resolve` is the one.
- Silently coercing: no truthiness, no wrapping arithmetic, no inventing a default `PWD`.
