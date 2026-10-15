# The rules every bug-bash agent follows

You hunt for defects in one subsystem of Nitrox as `main` has it. There is no diff and no author:
what you find goes to the maintainer, who triages it.

## What a finding is

A defect in the code as it stands, reachable from outside the code that has it — a syscall, a
message from another process, a device, a disk, a person at the keyboard: a crash, a hang, a wrong
result, lost or corrupted data, a leak, or a way past a capability or out of a namespace.

Not a finding: style; a missing comment or test of something that works; a doc that drifted; a
design you would have chosen differently; or a deferral already recorded — a `TODO(tag)` whose
entry in `docs/rationale/deferred-decisions.md` describes it. The scope lists those.

**Severity**, one of:

- `high`: a crash or hang of the kernel or a critical service (`init`, `service-mgr`, the storage
  service, a filesystem server), data lost or corrupted on disk, or authority gained — reachable
  by a process, a device or a disk the system does not trust.
- `medium`: a wrong result, a crash of a non-critical program, or a leak, on a reachable path.
- `low`: the rest worth knowing.

## Start at the edges

Start at the entry points and trust boundaries the scope names and follow the data inward, out of
the subsystem's paths wherever it leads. Read whole functions, their callers and the types
involved. Read the `CLAUDE.md` of every directory you work in: several hold rules for that crate
alone.

Judge against the docs that describe today: `docs/spec/`, `docs/architecture/`,
`docs/conventions/`, `docs/reference/`. `docs/design/`, `docs/planning/` and `docs/archive/` do
not, and `docs/decision-log.md` is a dated record, true as of each entry.

Do not read `docs/planning/bug-bash.md`, a PR's comments or a review: a hunt led by an earlier one
finds what that one found. The maintainer removes duplicates.

## Demonstrate or label

A confidently wrong finding costs the maintainer a reproduction and teaches them to skim you. For
each finding, either:

- **demonstrate it**: write a host test that feeds the input, break the code, point at the path,
  and say what you ran and what happened; or
- **label it** `unverified:` and say what would confirm or kill it.

Every finding has a file and line, one sentence on the defect, and a failure scenario: the inputs
or state, then the wrong behaviour. Without a scenario it is not yet a finding.

## Builds

- Run one test, not a suite: `cargo test -p <crate> --lib <name>` in `userspace/`, `cargo test
  --lib --target x86_64-unknown-linux-gnu <name>` in `kernel/`, `cargo test <name>` in `tools/`.
- Boot QEMU only when your instructions say you may: one agent at a time can, or the gates' timing
  fails them.
- Your worktree is yours alone. Restore every probe — `git checkout -- <file>` is safe there —
  and check `git status` is clean before you return.

## What not to do

- Flood: twenty notes bury the one bug. A dozen findings is a lot; cut what you would not raise in
  person.
- Report what the code could do better rather than what it does wrong.
- Leave a probe in place.
