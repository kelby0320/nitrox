# The rules every safety-survey agent follows

You survey one subsystem of Nitrox as `main` has it. There is no diff and no author: what you
return goes to the maintainer, who triages it into the plan. The survey looks for one class of
defect, a size, an ownership or a lifetime held by convention where no type holds it, and for the
`unsafe` that could be removed, narrowed or put behind an abstraction. **It is wide by design**: be
thorough, and cut only what you would not raise in person.

## Two kinds of item

### A defect

Wrong code as it stands, in the class above: memory written past what was allocated for it, read
before it was written, or used after it was freed; a handle, a mapping or a kernel object leaked,
released twice or used after release; an `unsafe` block whose `SAFETY` argument does not hold; a
safe function whose signature lets a caller break what the `unsafe` beneath it needs; a data race.

**A latent defect counts.** An overrun nothing reaches today, or a contract held only by the order
calls happen to come in, is a defect if one ordinary change would make it bite: say which change.
desktop-session-mgr received into a 256-byte buffer the kernel filled with 4,096 bytes, over the
heap's state, and did no harm only because its receives ran before anything allocated.

**Severity**, one of:

- `high`: memory corrupted, freed memory used, or authority gained, or the kernel or a critical
  service (`init`, `service-mgr`, the storage service, a filesystem server) crashed — reachable by a
  process, a device or a disk the system does not trust.
- `medium`: the same reachable only by a trusted party (a supervisor, a system server); a leak on a
  reachable path; or a latent defect one ordinary change would make `high`.
- `low`: the rest worth knowing.

### An opportunity

Code that is correct today but holds its safety by convention where a type, a narrower `unsafe` or
an abstraction could hold it. **One opportunity is a pattern, not a site**: every site the same
change would reach, counted, with three or four representative `file:line`s. Each says:

- **its kind**: `remove` (a safe way already exists), `narrow` (the `unsafe` covers more than the
  operation that needs it, or an `unsafe fn`'s contract could be a type), or `encapsulate` (one
  abstraction would hold the invariant for every site);
- **the abstraction**, as a signature or a sentence, and **its home**: the crate or module. The
  intended home for userspace's syscall wrappers and ownership types is libos
  (`docs/architecture/libos.md`): `no_std`, no `alloc`, typed `Handle<T, M>`, a host-test seam;
- **what it makes impossible**: the defect class it closes, best one this subsystem shows;
- **what it depends on**: Part E's syscall changes, another opportunity, the std port, or nothing;
- **value**: `high` (it closes a class of defect seen in this codebase, or removes most of the
  subsystem's `unsafe` of its kind), `medium` or `low`;
- **effort**: `small` (within a PR of other work), `medium` (a PR of its own), `large` (a part of
  the plan).

### Not an item

Style; a `SAFETY` comment missing where the argument is sound (CI requires them already); a doc
that drifted; `unsafe` that is inherent — inline assembly, MMIO, the user-memory copy primitives
themselves, a `#[repr(C)]` ABI type — unless its scope could be narrowed; or a deferral already
recorded: a `TODO(tag)` whose entry in `docs/rationale/deferred-decisions.md` describes it. The
scope lists those.

## Start at the boundaries

Start at the entry points, the boundaries and the inventory the scope gives, and follow the data
and the pointers out of the subsystem's paths wherever they lead. Read whole functions, their
callers and the types involved. Read the `CLAUDE.md` of every directory you work in: several hold
rules for that crate alone, and some explain an `unsafe` you would otherwise call needless.

Judge against the docs that describe today: `docs/spec/`, `docs/architecture/`,
`docs/conventions/`, `docs/reference/`. `docs/design/`, `docs/planning/` and `docs/archive/` do
not, and `docs/decision-log.md` is a dated record, true as of each entry. Do not read
`docs/planning/bug-bash.md`, `docs/planning/safety-survey.md`, a PR's comments or a review: a hunt
led by an earlier one finds what that one found.

## Demonstrate, label or count

A confidently wrong item costs the maintainer a reproduction and teaches them to skim you.

- **A defect** is demonstrated — a host test that feeds the input, a break of the code, Miri on one
  test, a path pointed at, and what you ran and what happened — or labelled `unverified:` with what
  would settle it. It has a file and line, one sentence on the defect, and a scenario: the inputs or
  state, then the wrong behaviour.
- **An opportunity** is counted: the number of sites, and the command that counted them. Check the
  abstraction fits three of them before proposing it.

**Miri** is installed for the userspace toolchain, `nightly-2026-07-20`. It shows undefined
behaviour in host-testable code: run one test under it, never a suite, as
`MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly-2026-07-20 miri test -p <crate> --lib --target
x86_64-unknown-linux-gnu <name>` in `userspace/`, or `... miri test --lib --target
x86_64-unknown-linux-gnu <name>` in `kernel/`. It is 10 to 100 times slower than a native run, and
cannot run inline assembly, so nothing that makes a syscall.

## Builds

- Run one test, not a suite: `cargo test -p <crate> --lib <name>` in `userspace/`, `cargo test
  --lib --target x86_64-unknown-linux-gnu <name>` in `kernel/`, `cargo test <name>` in `tools/`.
- Boot QEMU only when your instructions say you may: one agent at a time can, or the gates' timing
  fails them.
- Your worktree is yours alone. Restore every probe — `git checkout -- <file>` is safe there — and
  check `git status` is clean before you return.

## What not to do

- List sites as items: one opportunity per pattern, its sites counted.
- Call inherent `unsafe` an opportunity.
- Propose an abstraction you have not checked against three of the sites it claims.
- Leave a probe in place.
