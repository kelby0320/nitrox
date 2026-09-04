# Desktop entries

**Status:** Pre-stabilization. Introduced with display-arm Milestone 14 Part H (2026-09-04);
see [`display-arm-plan.md`](../planning/display-arm-plan.md). Two keys, both required. The
format is expected to grow an `icon` key when an icon set exists, and nothing else is planned.

A **desktop entry** is a package's declaration that one of its programs is an application — a
thing a person launches — and what to call it.

## Why it exists

The applications modal used to list `/bin`, which is every program the profile projects:
services, servers and command-line tools alongside the three windowed applications. Filtering
that list needs a fact that **cannot be read off a binary**. "Is this graphical?" is not a
property of an ELF; it is a claim somebody has to make, and an entry is where it is made.

The display name is the second half and matters as much. A modal listing `nxfiles` names a
binary. One listing "Files" names an application.

## Format

One file per application, TOML, in a package's `applications/` directory:

```toml
# nxterm.toml
name = "Terminal"
exec = "nxterm"
```

| Key | Meaning |
|---|---|
| `name` | What a person sees. Required, non-empty. |
| `exec` | The program to spawn, resolved through `/bin` like any other. Required, non-empty. |

Values must be double-quoted. `#` begins a comment. An entry missing either key, or with an
empty value for either, is **skipped with a line on the console** — one malformed entry loses
its own application, not everybody's.

The file's own name is not significant; `exec` is what gets spawned. By convention it is
`<exec>.toml`.

## Where they live, and how a session sees them

Entries sit **beside the binaries they name**, in the same store package:

```
/store/<hash>-coreutils-0.1.0/
    bin/           nxterm, nxfiles, nxedit, …
    applications/  nxterm.toml, nxfiles.toml, nxedit.toml
```

The profile server projects `applications/` at `/applications` exactly as it projects `bin/` at
`/bin` — merged across the profile's packages in manifest order, first provider wins. A session
gets it from one extra `sys_ns_bind`. **Both binds carry a subtree base**, so the first component
of the forwarded suffix names the projection — there is no bare form, and therefore no path
through `/bin` that reaches this one. See
[`profiles-and-namespace-projection.md`](../architecture/profiles-and-namespace-projection.md).

**A package carries its own applications**, which is the point of putting them here rather than
in a file the image build writes: installing a package brings its entries and removing one takes
them away, with no list anywhere that has to be kept in step.

## What an entry is not

It is not a capability. Listing an application does not grant the right to run it, and running
one does not require an entry — `/bin/nxterm` resolves whether or not `nxterm.toml` exists. An
entry answers "what should the launcher show", and nothing else. Authority remains where it
always is: in what a namespace contains.
