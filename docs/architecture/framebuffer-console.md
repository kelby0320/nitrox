# The framebuffer console

**Status: built with Phase 5 Part B; last checked 2026-09-14.** Everything COM1 receives is also
drawn on the screen from the first line of `kernel_main` until a client is handed
`/dev/framebuffer`, and again when the machine stops. Gated by `cargo xtask check-fbcon`, which
boots with no serial port and reads the screen back as text. Not built: colour, scrollback, any
input, and a console for a machine whose firmware leaves no linear framebuffer.

## Why it exists

The target laptop has **no serial port**. Before this, `kprint` reached COM1 and the log ring
(`kernel/src/klog.rs`), and the ring can only be read once userspace is up to read it — so a boot
that died before `init`, or in `init`, was a black screen. Every gate the project owns drives the
guest over COM1, and none of that survives contact with that machine.

It replaced a boot banner (a Nitrox tank decal, drawn once after userspace had been spawned),
which on a working boot showed for a fraction of a second and on a failing one showed nothing
useful.

## What it shows

**The byte stream COM1 receives, from both writers**: the kernel's `kprint!` / `kprintln!` and
the panic and fault paths (all through the serial `write_str`, `kernel/src/arch/x86_64/serial.rs`),
and userspace's `sys_kprint` (`kernel/src/syscall/table.rs`). The second is deliberate and differs
from the log ring, which keeps only the kernel's messages: a laptop whose `init` fails to mount a
root says so through `sys_kprint`, and a console showing only kernel lines would stop at
`init: spawned init (pid 1); handing off to userspace`.

The stream is decoded in `kernel/src/fbcon/text.rs`:

- **UTF-8, one cell per character**, carried across writes (a `kprintln!` arrives as several
  `write_str` calls). A character the face has is drawn; one it lacks, a broken sequence, or a
  stray continuation byte is drawn as `?`. About fifty string literals in the kernel carry a `—`,
  the first line of every boot among them, so this is not an edge case.
- **`\n`, `\r`, `\t`, backspace** move the cursor. A row that fills waits at the edge and wraps
  on the next glyph, so a line exactly as wide as the screen costs no blank row.
- **Escape sequences are swallowed** (`ESC [ … final`, and `ESC` plus one byte). A console with
  one colour has nothing to do with them, and drawing `[1;32m` mid-line is worse than dropping it.

## Glyphs

`kernel/src/fbcon/glyphs.rs` embeds `assets/fonts/Lat15-Terminus16.psf` — Terminus Font, 8×16,
normal weight, in the PSF1 form Debian's `console-setup` ships — whole, and reads glyphs out of it
in place. There is no generated table: the file's shape (256 glyphs, 16 rows, a Unicode table,
printable ASCII at its own index) is asserted when the kernel compiles, and ASCII is looked up by
index while anything else searches the table. The licence is SIL OFL 1.1, in
`assets/fonts/LICENSE-Terminus.txt` with where the file came from.

A bitmap, in a project whose userspace draws only TrueType, because this has to draw before there
is an allocator or a filesystem. The face was chosen from a rendered comparison with the other
two console-setup 8×16 faces and with DejaVu Sans Mono thresholded to one bit, which was visibly
the worst of the four at this size (decision log, 2026-09-14).

## Layout and scrolling

The grid is anchored at the top-left pixel, one cell per 8×16 block scaled by a whole number:
**the smallest scale at which the grid fits the static cell storage** of 256 × 100 cells
(`Geometry::for_screen`). The storage is static because the console draws before the allocator
exists; the scale falls out of it, so 1366×768 is 170×48 at scale 1 and 3840×2160 is 240×67 at
scale 2 rather than 480 columns too small to read.

**Rows are a ring**: a scroll moves the top index and clears what it exposes, so it copies no
cells. That matters after the yield, when every line anyone prints still lands in the grid and
nothing is drawn.

**Painting compares against what is on the screen** (`Console::shown`), so a write paints the
cells it changed and a scroll repaints the cells whose glyph moved. **The screen scrolls a quarter
at a time**, not a line: measured under TCG from the first line on screen to `compositor: up`
(about 90 lines), 536 and 568 ms jumping a quarter against 773 and 814 ms a line at a time. The
newest line is on the screen before the write that produced it returns either way.

## Who owns the screen

`kernel/src/fbcon/mod.rs` holds one `Console` behind an `IrqSpinLock`, and an owner:

| From | Owner | What a write does |
|---|---|---|
| `fbcon::init`, the first thing `kernel_main` does after checking Limine's base revision — before serial, the CPU tables, ACPI or PCI | the kernel | updates the grid and paints what changed |
| the first `/dev/framebuffer` handout (`framebuffer_server`, `kernel/src/object/kernel_server.rs`) | userspace | updates the grid; paints nothing |
| `stop_the_machine`, for a panic and a fatal fault alike | the kernel, for good | nothing more is written; the reclaim repaints the grid once |

**The handout, not the first frame, is the hand-over.** The phase plan said "until userspace first
commits a frame", and the kernel cannot see that: a client writes straight into an aperture
mapped into its address space. It does see the handout, and the yield happens under the console's
lock before the handle exists, so no paint can be in flight by the time a client could draw. That
is also the synchronisation the old banner lacked — it cleared the whole screen with nothing
ordering it against `display-selftest`'s first frame.

**The reclaim runs last in `stop_the_machine`**, on both of its branches: after the diagnosis has
been printed (its lines are the grid's last rows) and after every other CPU has been sent its stop
NMI, so a compositor stops drawing over the repaint. Taken back from userspace it repaints every
cell and clears the margins, since nothing about the screen is known any more.

## Locking on the path that most needs to finish

`LockRank::Fbcon`, just below the log ring and for the same reason: it is taken from the serial
`write_str` with `SERIAL` held (see `kernel/docs/lock-ordering.md`). Three things could hang an
ordinary lock on a path whose job is to reach the halt:

- **A fault inside the console on this CPU**, whose dump tees straight back in. `HOLDER` records
  which CPU is inside, and a re-entry returns at once.
- **A machine that is stopping**, whose other CPUs may have been halted holding the lock. Writes
  stop at `arch::stopping()`, and the reclaim waits a bounded number of spins and then leaves the
  screen as it is.
- **A long write on another CPU while a fault dump is teed.** A bounded wait, then that line is
  dropped from the screen — never from COM1.

The yield waits without a bound, because a yield that gave up would leave the console drawing
over the desktop, and none of the three can reach it.

## The gate

`cargo xtask check-fbcon` boots the release userspace over a kernel built with the `crash-key`
feature, with **`-serial none`**, and reads every screendump back into text with `glyphs.rs` and
`text.rs` compiled into `xtask` by path — the same face, palette and geometry. It claims:

1. **The boot is on the screen**: `Nitrox kernel — diagnostics online` (the first line, and an em
   dash from the Unicode table), `init: spawned init (pid 1); handing off to userspace`,
   `init: mounted fs-server-ext4 at /` (a `sys_kprint` line) and `compositor: up`, the last before
   the handout. Every row read in any dump is kept; measured on the day it landed, every line
   from the first to the handout was caught under both TCG and KVM.
2. **The console lets go**: for six seconds after the compositor draws, while every service keeps
   printing, no screendump contains a single ink-on-paper glyph cell.
3. **A stop takes the screen back**: F10 panics the `crash-key` kernel from inside the i8042
   driver; the screen becomes console text ending in `*** KERNEL PANIC ***` and the message, and
   after the pointer is moved it is unchanged two seconds later.

**Why a kernel feature for the third.** QEMU's `inject-nmi` reaches the guest through LINT1,
which nothing unmasks; it had no effect under TCG or KVM. Nothing in a working kernel stops on
demand, so the key is compiled in only for this gate, as `no-ps2-irq` is for `check-input`.

**Each claim was failed on purpose before it was trusted**: removing the `sys_kprint` tee
(claim 1: `compositor: up` never appears), removing the yield (claim 2: 344 glyph cells reappeared
over the desktop), removing the reclaim (claim 3: no panic on screen), and breaking the table
lookup (claim 1: the em-dash line never matches). Claim 3's last check did **not** fail at first
with the stop's NMIs deleted, since an idle greeter draws nothing for seconds; the pointer motion
is what makes a still-running compositor show itself.

## Not built

- **Colour or attributes.** One ink, one paper. A panic is not highlighted.
- **Scrollback.** What scrolled off is on COM1 and, for the kernel's lines, in `/dev/log`.
- **Anything for a machine without a linear 32-bit framebuffer.** `init` returns `None` and says
  so on COM1, which on such a machine nobody reads.
- **Cache attributes for the aperture.** The console writes through Limine's higher-half mapping
  as it stands; Phase 5 Part G is what decides what that mapping should be on real hardware.
