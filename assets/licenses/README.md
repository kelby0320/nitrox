# Licences of the crates the image carries

Each directory holds the licence files of one crates.io crate that a program in the image links,
copied verbatim from the crate's published package at the version `userspace/Cargo.lock` pins. The
image build stages them on the root filesystem at `/system/licenses/<crate>/`, beside the project's
own `LICENSE` at `/system/licenses/nitrox/`, since each licence asks that a copy of the software
carry it. A crate offered under a choice of licences ships every text it publishes.

| Crate | Version | Licence |
|---|---|---|
| `ab_glyph` | 0.2.32 | Apache-2.0 |
| `ab_glyph_rasterizer` | 0.1.10 | Apache-2.0 |
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| `core_maths` | 0.1.1 | MIT |
| `libm` | 0.2.16 | MIT |
| `miniz_oxide` | 0.9.1 | MIT OR Zlib OR Apache-2.0 |
| `owned_ttf_parser` | 0.25.1 | Apache-2.0 |
| `ttf-parser` | 0.25.1 | MIT OR Apache-2.0 |

**The lockfile is the list.** `cargo xtask test` fails when `userspace/Cargo.lock` names a crate
from a registry with no directory here, or a directory here names a crate it no longer does, and
the image build fails the same way. A new dependency's texts come from its package
(`~/.cargo/registry/src/*/<crate>-<version>/`): its `LICENSE*`, `COPYING*` and any `NOTICE`.

The fonts keep their licences beside them in [`assets/fonts`](../fonts), and the bootloader's
travels from its release tarball to the ESP, at `/boot/limine/LICENSE`.
