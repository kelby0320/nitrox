# userspace/nxsh/CLAUDE.md

`nxsh` — the Nitrox shell. Loaded when Claude Code reads files under `userspace/nxsh/`.

## What this is

The shell and its language: lexer, parser, evaluator, generic value operators, regex, and
the interactive loop. Scripts are `.nx`.

**It is the login leaf** as of 2026-07-31 — `session-mgr` spawns it into the constructed
session namespace with empty syscaps. It replaced the throwaway `usersh`, which is gone.

- Design (semantics/grammar): `docs/spec/shell-language.md`
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

- **Every scanner path must advance `pos` or return an error. A token that consumes
  nothing is an infinite loop, not a wrong token.** This has now bitten twice, in two
  modes. `is_ident_start` and `is_ident_char` are different sets — `$` starts an identifier
  (`$env`, `$last`) and does not continue one — so `scan_ident` must begin *past* the first
  byte. And in word mode, `]`/`)`/`}` are structural but their **openers** are not, so
  `scan_bareword` at `[` scanned zero characters and left `pos` alone; the argument loop
  then bumped the same empty `Word` forever, and `list [x]` locked the shell up. An empty
  bareword is now an error, which makes the hang impossible for *any* character rather than
  for the ones somebody thought of. The symptom is always a hung test run rather than a
  failure, which is what makes it expensive — the second one was found only because a new
  operator (`in`) could reach word mode for the first time.
- **A bareword argument means different things per operator.** `sort size` names a column;
  `display files` names a binding. Arguments are kept unevaluated and each operator
  decides, because only the operator knows.
- **A builtin's argument is a path, not an expression.** `cd ..` and `cd /system` both
  choke in expression mode, where `..` is a range and `/system` a division.
- **`run_line` is a block, and must be treated as one.** `exec` deliberately does nothing
  for `Stmt::Def` — definitions are registered by `hoist_defs`, which only ran from
  `exec_block`. So a whole script hoisted and a REPL line did not: a `def` typed at a
  prompt vanished and the next line said no such function. Anything `exec_block` does to a
  list of statements, `run_line` owes them too.
- **`should_display` must agree with `is_expression_shaped`.** They are the same question
  asked twice — "does this statement produce a value?" — and they disagreed: `if` counted
  for a block's value but was not echoed at the prompt, so the REPL computed a result and
  dropped it. If you extend one, extend the other. (`try` used to be on both lists; it is
  an expression as of v1.2, so it arrives as `Stmt::Expr` and needs no entry.)
- **The interactive loop in `main.rs` intercepts nothing.** It used to match one line —
  `exit` — because ending the loop was the one thing `run_line` could not do. That was
  still a second implementation of the language, and it behaved like one: `exit 1` missed
  the string comparison and came back as "`exit` is handled by the shell's driver", so a
  script could not set its own status. A `cd` guard had sat beside it for weeks for the
  same reason, refusing a builtin the interpreter had implemented. Since v1.2 `exit` is a
  real builtin whose status travels the error channel (`EvalError::is_exit`), and the loop
  hands every line to `run_line` unread. **Do not add a case here.**
- **`Host::exists` and `list` must ask the namespace the same question.** A path can be
  real in three ways: it names a binding (or sits above one), it resolves to an object, or
  a directory session opens it. `exists` had only the last two, so `cd /` and `cd /bin`
  were refused while `list` showed both — `list` walks the bindings (`SYS_NS_ENUMERATE`)
  and `cd` did not. The ancestor test compares against `path + "/"`, or `/bin` would make
  `cd /binary` succeed.
- **`list /` is the root directory, not a division sign.** `/system` lexes as one path
  word, so only a *lone* `/` reaches the parser as `Tok::Slash` — which is why this parsed
  as division through every test until someone typed it at a real prompt. A lone `Slash` in
  argument position means the next thing is whitespace or a closer, so there is no right
  operand and division is impossible. Whenever you touch `starts_an_argument`, the test for
  `list /` and the test for `6 / 2` only mean something as a pair.
- **A new token that can end a statement must join `Tok::ends_statement`.** It is a
  whitelist of *enders*, so anything new defaults to "continues" — the lexer then swallows
  the newline after it and the parser reports "expected a newline between statements" on
  the *following* line. `break` and `continue` landed this way: `for x in xs { break }`
  parsed (a `}` follows), and `break` on its own line did not. The default is the right one
  — a wrong "ends" silently splits one statement into two — but it means the symptom points
  one line past the cause.
- **The shell does not rewrite a spawned stage's paths.** It passes `argv` through as
  written and hands over the same `PWD`, so both sides resolve identically. Pre-resolving
  would reintroduce the split-brain the environment design exists to prevent.

## Testing the interactive path

`run_line` is library code, so a REPL *session* is host-testable: the `repl(&[...])` helper
in `eval.rs`'s tests drives a sequence of lines through one interpreter, which is what a
person at a prompt actually does. **Use it for anything that spans two lines.** Every test
before it fed the evaluator a whole script through `run`, which is why three
interactive-only bugs got in — the stale `cd` guard, `list /`, and `def` hoisting.

What it does *not* cover is the console loop in `main.rs`: byte reading, backspace, Ctrl-D,
the prompt itself. That needs a driven console — see `TODO(nxsh-console-tests)`.

## Forbidden patterns

- Syscalls in the library half (use `Host`).
- A second path-resolution implementation — `librsproto::path::resolve` is the one.
- Silently coercing: no truthiness, no wrapping arithmetic, no inventing a default `PWD`.
