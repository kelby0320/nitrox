# Comments and file size

Two rules about the shape of the Rust code, each held by a check in CI.

## Comments

**A comment says what the code cannot**: a non-obvious why, an invariant, a `SAFETY` argument.
What was done, when, and in which PR, review, phase or part belongs in the commit message and
`docs/decision-log.md`, where it stays true; in the code it goes stale.

**`cargo xtask check-comments [--base REF]`** measures a branch from its merge base with `REF`
(`origin/main` by default), from whatever directory it is run in:

- A **comment line** is one whose trimmed text starts with `//`, doc comments included; a **code
  line** is any other line that is not blank.
- A line is **new** if the branch adds it and does not remove the same trimmed text elsewhere: a
  moved or re-indented line is not new. Untracked Rust files count whole.
- **New comment lines may be at most 0.15 of new code lines, plus 3.** A change that adds no
  more comment lines than it removes passes whatever its ratio.
- **A `// SAFETY:` comment and a doc comment's `# Safety` section are neither comment nor code**:
  the rules require both. A citation in one still fails.
- **A new comment citing history fails**: a PR (`PR #12`, `PR#12`, `(#123)`), an issue, a ticket,
  a review finding, a slice, a phase (`Phase 6`), a part (`Part H.4`, `Part K's`, `Parts A–G`), a
  milestone (`Milestone 7`, `M14 Part`), also when the citation is wrapped across two lines.

Each failure names the file and line and says how to fix it.

**The edit hook.** Every Claude Code session in this repository runs
`tools/target/release/xtask hook-comments` after each `Edit` or `Write` of a Rust file
(`.claude/settings.json`). When the edit adds more comment lines than it removes, three or more
of them new and over **0.1** of its new code, or when a new comment cites history, it says so to
the author; it never blocks. It speaks at a lower bar than CI fails at so a comment is cut as it is
written rather than at the gate. It is silent until a `cargo xtask` command has built the release
binary.

## File size

**A Rust file stays at 1,000 lines or fewer.** Split one that grows past it by responsibility,
with its tests in a sibling `tests.rs`.

The files over it when the rule came in are listed in `tools/xtask/size-baseline.toml` with their
lengths, and **an entry only goes down**. **`cargo xtask check-size`** fails a file over 1,000
lines that is not listed, a listed file longer than its entry, and an entry that is stale — its
file shorter, at 1,000 lines or fewer, or gone — until it is brought down:

- `cargo xtask check-size --update` lowers each stale entry to its file's length and removes
  those at 1,000 lines or fewer or gone. It never raises an entry or adds one.
- `cargo xtask check-size --rename OLD NEW` moves a renamed file's entry to its new path, at no
  more than its old allowance.
