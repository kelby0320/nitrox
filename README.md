# Nitrox

Nitrox is a hobby operating system written in Rust, for x86_64 machines that boot by UEFI. It is
the successor to [Latte](https://github.com/kelby0320/latte), an earlier Unix-like system in C.

## What it is, and what it rejects

Nitrox keeps what Unix got right: composable pipelines, system state as named resources, and a
powerful shell. It rebuilds them without Unix's weaker patterns:

- **No ambient authority.** Authority is a handle a process holds, never an identity it runs as:
  the kernel has no users, no UID or GID, and no root to become.
- **Per-process namespaces.** Each process sees its own tree of names. A sandbox is built by leaving
  things out of it, not by refusing access to them.
- **Asynchronous system calls.** Anything that can block returns a pending operation, and a thread
  waits on `sys_wait`, never inside another call.
- **No signals.** Events arrive on a notification queue.
- **No POSIX.** Filesystems are userspace servers, and the shell, `nxsh`, passes typed values
  rather than bytes.

The result is closer in spirit to Plan 9, Fuchsia and seL4's capabilities than to Linux or the
BSDs. aarch64 is designed in through the kernel's architecture layer, and not yet implemented.

## Where it stands

**Pre-v0.1.** Phases 0–6 are built:
- the kernel and its capability substrate;
- the services;
- a windowed desktop, with a graphical greeter, a compositor, a terminal, a file manager and an
  editor;
- ext4 and FAT, read and written, each by a server of its own;
- bare metal: the target laptop boots from its own disk, installed from a live USB stick;
- USB keyboards, mice and sticks.

The system call ABI, the wire formats and the kernel's internals are not yet stable. The plan is in
[`implementation-plan.md`](docs/planning/implementation-plan.md): the work before Phase 7, then a
port of Rust's `std`, networking, and a browser.

## Building it

It builds on a Linux host. The package names below are Debian's and Ubuntu's.

**Rust, by [rustup](https://rustup.rs).** The kernel and the tools build on an exact stable;
userspace builds on an exact nightly, used only to rebuild `core` and `alloc` for its hard-float
target (`-Z build-std`), never for a nightly language feature. Each is pinned in a
`rust-toolchain.toml`, and `rustup toolchain install`, run once in the repository's root and once
in `userspace/`, installs the one pinned there.

**The image tools and QEMU**, as CI installs them:

```
sudo apt install qemu-system-x86 ovmf gdisk mtools dosfstools e2fsprogs
```

`curl` fetches the pinned [Limine](https://github.com/limine-bootloader/limine) release the first
time an image is built. QEMU's UEFI firmware is looked for where the common distributions put it;
`NITROX_OVMF` names it otherwise. **`--kvm` is worth having**: it is faster, and needed with a QEMU
older than 9.0, whose software emulation lacks the x2APIC the kernel requires.

## Running it

Every command runs from the repository's root:

```
cargo xtask build          # the kernel
cargo xtask image          # the UEFI-bootable disk image
cargo xtask qemu --kvm     # boot it under QEMU (--grab hands it the pointer and keyboard)
cargo xtask image --live   # the live image, for a USB stick: it can install to a disk
cargo xtask help           # every command
```

The image boots to a graphical greeter. The build seeds an account, `alice`, whose password is
`DEMO_PASSWORD` in `tools/xtask/src/main.rs`.

## Testing it

```
cargo xtask test           # the host-side unit tests
cargo xtask test-qemu      # boot a self-test image headless; pass or fail by its exit code
cargo xtask test-interactive   # boot the release image and drive a login and a shell
```

Beyond those, the `check-*` gates boot an image and drive it as a person would: typing, clicking,
plugging a stick in, and reading the screen and the disk afterwards. What each holds is in
[`qemu-integration-tests.md`](docs/conventions/qemu-integration-tests.md).

## The documentation

Start with [`overview.md`](docs/architecture/overview.md), which is meant to be read in one sitting.
Then:
- `docs/architecture/` describes each subsystem as built;
- `docs/spec/` holds the exact contracts;
- `docs/rationale/` explains why things are the way they are;
- [`decision-log.md`](docs/decision-log.md) records each decision as it was made.

The project is developed with Claude Code. The rules its sessions work by are in the `CLAUDE.md`
files, beginning at the root.

## Licence

Nitrox is under the [MIT licence](LICENSE), all of it, the default wallpaper's photograph
included, except these, which keep their own:

- **The fonts in [`assets/fonts`](assets/fonts)**:
  - DejaVu, under Bitstream Vera's terms. Its notice travels with the faces into the image, at
    `/system/fonts`.
  - Terminus Font, under the SIL Open Font License 1.1. The kernel embeds it, and its notice sits
    beside the kernel on the boot partition.
- **The crates.io crates userspace links**, each under its own licence. The image carries their
  texts at `/system/licenses`, from [`assets/licenses`](assets/licenses/README.md), and
  `cargo xtask test` fails when a crate has none there. The build tools' crates run only on the
  build machine and ship nothing.
- **Limine**, the bootloader, under its BSD 2-clause licence, fetched from its release at build
  time. Its licence goes on the boot partition with it, at `/boot/limine/LICENSE`.
