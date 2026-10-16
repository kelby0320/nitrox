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
- **A `// SAFETY:` comment and a doc comment's `# Safety` section are neither comment nor code**,
  to their fifth line: the rules require both. Past it, the lines count as comment. A citation in
  one still fails.
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
lengths. **The tools only lower an entry; one goes up only by hand, in a reviewed commit of its own
that says why** — the 1,000 lines is a guideline held as a ratchet, so that nothing grows
unremarked. **`cargo xtask check-size`** fails a file over 1,000
lines that is not listed, a listed file longer than its entry, and an entry that is stale — its
file shorter, at 1,000 lines or fewer, or gone — until it is brought down:

- `cargo xtask check-size --update` lowers each stale entry to its file's length and removes
  those at 1,000 lines or fewer or gone. It never raises an entry or adds one.
- `cargo xtask check-size --rename OLD NEW` moves a renamed file's entry to its new path, at no
  more than its old allowance.
- `cargo xtask check-size --split OLD NEW...` moves a split file's entry onto its pieces: each still
  over 1,000 lines gets an entry at its length, and `OLD` keeps one at most its own. Refused if
  the pieces hold more than `OLD`'s entry, not counting each new file's leading doc, `mod`, `use`
  and attribute lines, or the `mod` line each needs in its parent.

**A split is moves only, and shown to be.** Each move is a commit of its own, before any
formatting, and **`cargo xtask check-moves [REV]`** fails a commit (`HEAD` by default) holding any
changed line that `git diff --color-moved=blocks --color-moved-ws=allow-indentation-change` does not
find moved — besides blank lines, `mod` lines and the `#[cfg(test)]` before one, `use` statements,
a visibility widened to `pub(crate)` or `pub(super)`, an `include_bytes!` or `include_str!` path
re-rooted to name the same file from the line's new directory — each with the lines running on
from it that are the same on both sides, a piece of the move too short for git to call moved once
the changed line splits it — and an inline module made a file: its `mod NAME {` replaced by `mod NAME;`, and its
closing `}`. A line-by-line match is not enough: a
`continue` deleted in one place and added in another passes one. Formatting follows in the next
commit.
