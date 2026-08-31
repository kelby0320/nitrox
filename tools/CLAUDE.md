# tools/CLAUDE.md

Host tooling. Loaded when Claude Code reads files under `tools/`.

## What this is, and which rules reach it

Everything here runs on the **host** and reaches no target: `xtask` orchestrates builds, assembles
the disk image, and drives QEMU for the gates. The root `CLAUDE.md`'s forbidden list is about the
*kernel*; `userspace/CLAUDE.md`'s dependency bar is about what ships in the image. Neither governs
a program that only ever runs on a developer's machine and in CI.

What does govern it is below, and it exists because `xtask` took its first crates.io dependency in
M11 Part A — until then "avoids external crates" was a note in `main.rs`, and a reader hit that
before they found any reasoning.

## External dependencies

**A host tool may take one.** The bar is lower than userspace's because nothing here is
redistributed, nothing is `no_std`, and nothing has to build for a custom target — but it is not
absent:

- **It must earn its transitive tree.** Count the tree before agreeing to it. `png` brings eleven
  crates; that was worth it for an image encoder and would not be for a string helper.
- **Permissive licence** — MIT, Apache-2.0, BSD, Zlib. Nothing here ships, so no notice travels
  with the distribution, but a build dependency with a copyleft licence is still a question
  nobody wants to answer later.
- **Prefer the narrow crate.** `png` rather than `image`: one format, decoded and encoded, versus
  a dozen formats and their dependencies.
- **A hand-rolled version is not automatically the better answer.** The M11 Part A decision-log
  entry is the worked example: a stored-block PNG writer is genuinely about a hundred lines, and
  those hundred lines stood between a judgement about how something looks and seeing it, in the
  part whose whole purpose was removing that cost.

**The lockfile is the pin.** `userspace/CLAUDE.md` asks for pinned rather than floating versions;
here that is `tools/Cargo.lock`, which is checked in. A caret requirement in `Cargo.toml` plus a
committed lock is a reproducible build; the `=` form as well would be belt and braces on a
workspace that ships nothing. **A dependency added without committing the updated lock is the
thing to catch in review** — it makes CI resolve something nobody tested.

## What `xtask` must keep being

- **A single `cargo run -p xtask <cmd>`**, with no setup step beyond a checkout and a crates.io
  fetch. Anything that needs a service running, a container, or a manual download is not a
  subcommand.
- **The place a gate's expected answer is *computed*, not stored.** `check-display` renders what
  the guest should show rather than comparing against a checked-in image, which is why `libui`,
  `libdraw` and `libterm` are dependencies here. A golden file would rot the first time a widget
  changed, and nothing would say whether the rot was the change or the file.
- **One source per expected answer.** `cargo xtask preview` and `check-display` render from
  `preview_frames`, so a preview cannot drift into being a picture of something the gate never
  demands. Two call sites that "obviously" build the same thing is how that drift starts (PR #261
  review).

## Tests

`cargo xtask test` runs the tools workspace first, so anything with a `#[test]` here is in the
host suite and gated by CI. Prefer a host test over a QEMU gate wherever the answer does not need
a guest: it costs a second rather than three minutes, and the whole reason `preview` exists is
that a cheap answer gets asked more often.
