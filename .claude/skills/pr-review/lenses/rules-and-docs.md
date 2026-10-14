# Lens: rules and docs

What the code must obey that no gate checks, and the docs the change makes false.

**Rules.** The root and directory `CLAUDE.md` files are the source; these are the ones CI cannot
see:

- A `SAFETY` comment on every `unsafe` block, saying why the precondition holds here, not that it
  does.
- `#[repr(C)]` on every type that crosses the kernel–userspace boundary, on both sides.
- No `panic!()` on the critical path: `init` and `eshell`.
- Doc comments on public items.
- Every `TODO(tag)` has an entry in `docs/rationale/deferred-decisions.md` that describes this
  deferral.
- A rule from a directory's `CLAUDE.md`, quoted exactly.

**Docs.** Does the change make a statement in `docs/spec/`, `docs/reference/`,
`docs/architecture/`, `docs/conventions/` or a `CLAUDE.md` false? Quote the statement and say what
the code does now. A new decision belongs in a new `docs/decision-log.md` entry, and a deferral
ended in `deferred-decisions.md`.
