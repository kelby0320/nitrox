# Nitrox Implementation Plan — Phase 9 — The browser

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. This phase is
**planned, not built** — nothing below describes current behaviour.

---

## Phase 9: The browser

**Goal:** the second north star — a web browser running on Nitrox.

**Agreed 2026-08-25 as "Phase 7"** and renumbered (decision log, 2026-09-10).

**It is a capstone, which is to say it is an integration test with a user interface.** A
browser exercises networking, TLS, threads, floating point and SIMD, graphics, fonts, memory
pressure and `std` simultaneously, which is why it was chosen as the thing to aim at: it is
hard to pass by accident. Everything it needs is built by Phases 5–8, and this phase is where
that claim gets tested.

### Strategy, settled 2026-07-20

**A hybrid, not a port of Servo.** Reuse the pure-Rust Servo crates — `html5ever`,
`cssparser`, `selectors` — with a pure-Rust JS engine (`Boa`, restricted subset). Porting full
Servo drags in SpiderMonkey, C, and a GPU story, which would force the POSIX C shim early and
make the browser a compiler-porting project rather than a browser project.

- [ ] A restricted HTML/CSS/JS engine on pure-Rust crates
- [ ] `rustls`-based HTTPS (from [Phase 8](phase-8-networking.md))
- [ ] Layout and painting onto `libui` / `libdraw`

### The parallel track: package management and system administration

**It blocks on nothing and nothing blocks on it**, which is why it is recorded here as an
opportunistic track rather than a phase. Pulled up from the Phase 3 backlog:

- [ ] Package manager daemon (list / add / remove store paths)
- [ ] Generation manifests plus atomic switch and rollback
- [ ] Store GC (mark reachable, sweep unreachable)

The content-addressed store already exists and already projects `/bin`; this is the
administration layer over it, and it becomes worth building the first time installing something
on the laptop is a thing somebody wants to do twice.

### Definition of Done

The browser renders and navigates a real page, over the network, on real hardware.

### What Phase 9 does not do

A modern web platform. No workers, no WebGL, no video, no extensions — the goal is a browser
that proves the system, not one anybody would switch to.
