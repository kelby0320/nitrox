# Nitrox: Display Substrate — Design Notes (v1)

## Status

The mechanism beneath the display: who owns the framebuffer, how a client's pixels reach it,
where input comes from, how text is drawn, and how any of it is tested. Settled with the
maintainer 2026-08-04.

**Companion documents.** `nitrox-ui-composition-model-v2.md` owns the layer above — what a
window *means*, how windows compose, desktops, templates. It was written first and settles the
semantics; this one exists because it specifies none of the mechanism, and where they touch (the
namespace shape for `/dev/draw`) v2 is authoritative. `nitrox-desktop-shell-v1.md` owns what a
user actually sees, and §4a, §4b and §5a below exist **because that document demanded them** —
roles, struts, capture and global hotkeys were all absent from the first draft of this one.

Nothing here is built. The build order is `docs/planning/display-arm-plan.md`.

## 1. What exists today

Worth stating, because three of these are load-bearing for decisions below:

- **`kernel/src/framebuffer.rs`** draws the boot screen into Limine's linear framebuffer —
  base pointer, width, height, pitch, and channel shifts — and is then **idle**. Nothing owns
  those pixels after boot; the console is serial.
- **`kernel/src/font.rs`** carries a hand-coded **8×16 bitmap font**, used by that boot screen.
- **No input driver of any kind.** No PS/2, no USB HID. Keyboard input reaches the system only
  as bytes on the serial console.
- **`DeviceNode` models char and block devices** (`try_new_char`, `try_new_block`). A
  *mappable* device is a third shape it does not have.
- **The tty server's `backend_write`** was built as the seam for exactly this: "serial today, a
  compositor surface later".
- **`xtask` has no QEMU monitor or QMP channel.** `test-qemu` adjudicates from an exit code;
  `test-interactive` drives serial text. Neither can see a pixel.

## 2. Two processes

**Compositor** — the framebuffer, surfaces, windows, input routing, focus. Serves `/dev/draw`.

**Desktop shell** — desktops, the wiring graph, templates, spawning applications. Serves
`/dev/desktop`, and is the only holder of application namespace handles (v2 §4).

**Rejected: one process.** Stacked up, the single-process version would own surfaces, windows,
ports, wiring, desktops and saved graphs. That is too much for the one process that must never
wedge — the compositor is what everything on screen depends on, and it should be small enough
to reason about and restartable last.

**Rejected: three processes** (a "display server" beneath the compositor, owning the device).
With no modesetting and one output, the device layer is "here is a pointer, a stride and a
pixel format". A process boundary there buys an IPC hop per frame and nothing else. It stays a
**module** boundary, so a second output or a real GPU can split it later without moving the
seam.

**No modesetting, deliberately.** Limine hands over a framebuffer the firmware has already
configured. Programming a GPU blind is where display bring-up goes to die, and nothing in the
desktop MVP needs a mode change.

## 3. The framebuffer

The kernel owns the physical mapping — it is the only thing that can, since Limine hands it
over before userspace exists — and exposes it to userspace as a **mappable resource**, bound
into a namespace. Whoever holds that binding is the compositor.

**Authority is the namespace grant.** This is the same rule v2 §3 gives for creating UI, and
the same one the fs-server and profile-server already live under: a process can drive the
display if and only if its namespace contains the binding. There is no display capability bit
and no registration call.

**Not a char `DeviceNode`.** Reads and writes are the wrong shape for a linear buffer that
wants to be mapped once and written directly. The kernel already has `MemoryObject` for
"memory you map"; the framebuffer is one of those whose frames happen to be a device aperture
rather than ordinary RAM.

**What the compositor needs to be told** is the geometry it cannot infer: width, height, pitch
(bytes per row, which is not width × bpp), and the channel layout. Limine reports all four to
the kernel; they cross to userspace as an attribute of the resource rather than a second
protocol.

## 4. Surfaces

A client draws into memory it allocates, and the compositor reads it. Concretely:

1. Client creates a `MemoryObject`, maps it, draws.
2. Client transfers the handle to the compositor over IPC — once, not per frame.
3. Client sends `Commit { buffer, damage }`.
4. Compositor composites the damaged region and, when it is done with that buffer, **releases**
   it back so the client knows it may draw again.

**Every syscall this needs already exists** — `sys_memory_create`, `sys_memory_map`, handle
transfer on an IPC message, and notifications. The surface protocol adds no kernel surface at
all, which is the strongest argument for this shape over the alternatives.

**Rejected: pixels over IPC.** Copying a frame through messages is the obvious non-starter, and
worth naming so nobody proposes it as a simplification.

**Rejected: server-allocated buffers.** The compositor handing out memory makes it the
allocator for every client's rendering, couples buffer lifetime to compositor policy, and buys
nothing here. Client-allocated is also the Wayland shape the Phase 4 plan already names.

**Release is the part to design carefully.** A client that redraws into a buffer the compositor
is still reading produces tearing that is invisible in testing and obvious in use. Double
buffering is the client's business; *knowing when it is safe* is the protocol's.

### 4a. Window roles, and reserved space

A window carries a **role**: `normal`, `panel`, `popup`, or `dialog`. It is not decoration —
each changes what the compositor does:

- **`panel`** — a bar. Occupies a screen edge, is always visible regardless of which desktop is
  current, and **never takes keyboard focus**, so clicking the clock does not steal input from
  the terminal.
- **`popup`** — a menu or a modal. Transient, parented to another window, and may extend beyond
  its parent's bounds. Menus force this: a menu clipped to its window is not a menu.
- **`dialog`** — parented, on its parent's desktop, listed but **not offered as a wirable node**
  on the composition canvas (v2 §6).
- **`normal`** — everything else.

**A panel reserves space.** A maximised window must not cover the bars, which X calls struts.
The compositor subtracts reserved edges from the area it offers to `normal` windows.

**Roles and struts must be settled before the window protocol is frozen** (plan Milestone 2).
Retrofitting a role into a shipped protocol touches every client.

### 4b. Thumbnail capture

The compositor can hand a client a **scaled snapshot** of another window's surface. The desktop
shell's overview is built on it (`nitrox-desktop-shell-v1.md` §6), and it is what lets the
overview be an image grid the shell draws rather than a transform pipeline inside the
compositor.

**Capability-gated, necessarily.** Giving a client another window's pixels is the leak the
composition model's namespace rule exists to prevent — the shell may do it because it holds
`/dev/draw` with rights an application does not.

**Captured at thumbnail size**, scaled once at capture. Snapshotting eight full-resolution
surfaces is tens of megabytes; scaling on the way out is a few, and it is the difference between
affordable and not on a software renderer.

## 5. Input

A PS/2 keyboard and mouse driver in the kernel — that is where AHCI lives, so it is the
existing rule rather than a new one — delivering **key events**, not bytes:

```
KeyEvent { keycode: u16, pressed: bool, modifiers: u16 }   // #[repr(C)]
```

**Scancode → keycode in the kernel.** It is one small table that every consumer would otherwise
duplicate, and getting it wrong is not a policy disagreement, it is a bug.

**Keycode → character in userspace.** A keymap is *policy and data* — layout, locale, dead keys,
compose sequences. It has no business in the kernel, and putting it there would make a keyboard
layout a kernel rebuild.

**Modifiers are in the event because the shell needs them.** Shift-Enter is a filed shell item
(§11b continuation) that today's byte stream structurally cannot express: `\n` is `\n` whatever
was held down. This is the mechanism that unblocks it, and it is the reason the boundary sits
at key events rather than characters.

**The compositor owns focus and routes.** It knows which window is focused because it owns
stacking and input; routing anywhere else would need a second copy of that state.

### 5a. Global hotkeys

A client may register a key combination that reaches it **regardless of focus** — the desktop
shell's Super key is the motivating case.

**This is a capability, not an ambient grab.** If any process could claim Super, any process
could impersonate the launcher; if any process could claim a printable key, any process could
keylog. Registration is gated the same way capture is: by holding a binding an ordinary
application was never given.

## 6. Text

**Start with the 8×16 bitmap font that already boots the machine.** It is in the tree, it costs
nothing, and it is enough for a terminal — the MVP flagship.

**This defers a real decision rather than making one by accident.** Scalable, antialiased text
wants a rasterizer (`fontdue`, `ab_glyph`), and **userspace has zero external dependencies
today** — every crate in the tree is first-party. That is a decision worth taking deliberately,
with its own discussion, and not one to make incidentally while trying to get the first pixels
on screen. Trigger: the toolkit needing text at more than one size.

## 7. Determinism

**Compositing must be deterministic**: the same surfaces, geometry, damage and stacking must
produce the same bytes, every time, on any machine.

This is a design constraint, not a testing preference — §8's gate is built on hashing the
result, and a compositor that varies by wall-clock, scheduling order, or uninitialised padding
cannot be gated at all. Concretely: no timestamps in rendered output, no reading padding bytes
in a surface, no dependence on the order in which clients happened to commit.

It is also a second argument for the bitmap font (§6): a glyph table hashes identically
forever, where an antialiasing rasterizer can change output across versions and turn a golden
hash into a maintenance liability.

## 8. The test gate

Every other part of this system ends green on a gate. Pixels have none today, and the display
arm is the largest subsystem in Phase 4 — settling this **before** the first commit rather than
after the terminal is the difference between a tested subsystem and an untested one.

### 8a. Host tests carry correctness

Almost all of this is pure logic and belongs behind a trait, exactly as the ext4 parser sits
behind `BlockReader` and the interpreter behind `Host`:

- damage rectangles, clipping, stacking, focus policy
- glyph layout and the text/ANSI render path
- keycode → character mapping
- the wiring graph, template instantiation and extraction (v2 §7)

The piece that *looks* like it needs a screen is compositing, and it does not. A
`Framebuffer` trait — base, width, height, pitch, format — with a real implementation over
Limine's mapping and an **in-memory one for tests** makes "composite these surfaces with this
damage" a pure function, assertable pixel-exactly in milliseconds.

### 8b. The guest proves the plumbing

`test-qemu` gains a display self-test: the guest composites a **known scene**, hashes its
framebuffer, and reports the hash. The value it must equal is a constant the *host* test also
asserts against its in-memory composite of the same scene.

**The same hash asserted in two places** is what makes this worth running. If the host and the
guest disagree, one of them is wrong and the commit that broke it is the one that fails —
rather than the two quietly diverging until something visible breaks months later.

The verdict travels the existing `isa-debug-exit` path; no new harness mechanism.

### 8c. `screendump` proves the device binding

A compositor can hash its own buffer correctly while writing to the wrong base address, the
wrong stride, or with the channels swapped. **Nothing inside the guest can detect that** — it is
perfectly consistent with itself.

So: QEMU `screendump` over a monitor/QMP socket, compared against a reference image. Run as a
**smoke gate, once per display-arm change**, not per commit: it is slow, image comparison is
brittle, and it covers a narrow class of bug that changes rarely.

The division worth remembering: **8b tests the compositor, 8c tests the framebuffer binding.**

### 8d. Input injection, which is needed anyway

Keymap translation is a pure function and host-tested. End-to-end input has the mirror problem
— nothing can *type* at a GUI — and QMP's `input-send-event` injects key and mouse events
directly.

Worth noticing now: **`test-interactive` types over serial today, and that stops working the
moment the shell sits behind a GUI terminal.** The harness needs this channel regardless, so
adding QMP for `screendump` pays for itself twice.

### 8e. The first milestone

Not "draw something on screen":

> **The compositor composites a known scene, and the host test and the guest agree on the hash.**

That is provable before there is a window, a client, a font, or a terminal — and it puts the
gate in place before there is anything to regress.

**Choosing the reference scene matters more than it sounds.** It wants overlap, clipping at a
screen edge, and a non-trivial stride. A solid-colour fill would hash fine and prove nearly
nothing.

## 9. Open questions carried forward

- **Buffer release semantics** (§4) — the protocol shape is settled, the exact release
  signal is not. Trigger: the first client that double-buffers.
- **Mouse events** (§5) — the keyboard side is specified; pointer events, coordinates and
  button state are not. Trigger: the compositor needing to move a window.
- **USB HID** (§5) — PS/2 is what QEMU gives us. Real hardware is later, and the key-event
  boundary is chosen so that the driver changes and nothing above it does.
- **A font rasterizer, and with it the first external crate in userspace** (§6). Trigger:
  text at more than one size.
- **Multiple outputs** (§2) — the module seam exists for it; nothing is designed.
- **Live thumbnails** (§4b) — frozen is the v1 answer. Trigger: the frozen ones being visibly
  wrong in use.
- **What a `panel` does on a multi-desktop system** (§4a) — bars are always visible, which is
  simple; a panel that belonged to one desktop would need a rule nothing has yet.
- **Reference-scene contents** (§8e).
