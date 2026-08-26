# Nitrox: The Graphical Session

## Status

**Built, and checked 2026-08-25** — Milestone 7 (Parts A–F). Graduated from `design/` on
2026-08-25, revision 2.

What exists: [`auth-service`](../../userspace/auth-service) answers `Auth::Authenticate` at
`/svc/auth`; [`desktop-session-mgr`](../../userspace/desktop-session-mgr) draws the greeter,
authenticates through it, builds the session namespace and spawns the session leader;
[`desktop-shell`](../../userspace/desktop-shell) is that leader — top bar, applications modal,
a namespace constructed per application, window placement. The two supervisors share
[`libsession`](../../userspace/libsession), so the serial and graphical columns authenticate
and build namespaces through one implementation. `cargo xtask check-login` boots a **release**
image, refuses a wrong password, logs in, launches a terminal, and proves a serial login runs
beside it.

What is **not** built, and is named as such below: §6.3's session services (nothing exists that
must outlive or precede the shell), and the multi-desktop model
[`ui-composition-model.md`](../design/ui-composition-model.md) describes — the shell ships one
implicit desktop, and the overview and desktop indicator are Milestone 8.

Originally written 2026-08-12, from a gap found while planning Milestone 5 Part C: the plan had
`session-mgr` spawning `nxterm`, and the maintainer's question — *"nxterm is a desktop app the
way gnome-terminal is; should session-mgr really be spawning it?"* — turned out to have no
answer anywhere in `docs/`. A grep for *display manager*, *graphical login*, *greeter* returns
nothing. [`ui-composition-model.md`](../design/ui-composition-model.md) §6 assigns "spawning applications"
to the desktop shell and §5a says the shell "holds a full-rights handle to every application's
namespace because it *created* those namespaces at spawn" — but nothing says who authenticates
a graphical user, or who spawns the shell. **The top of that column was empty.** This document
fills it.

## 1. Two stacks, not one

The system has two ways for a human to arrive, and they are parallel rather than nested:

| | serial / (future) SSH | graphical |
|---|---|---|
| **authenticates** | `session-mgr`'s `login:` prompt | `desktop-session-mgr`'s login **window** |
| **session leader** | `nxsh` | `desktop-shell` |
| **presentation** | a `Tty` it opened | a window on the compositor |
| **Unix analogue** | `getty`/`login`, `sshd` | `gdm` → `gnome-session` → `gnome-shell` |

`session-mgr` is **not** the parent of the graphical stack, and does not become one. It is the
serial column's supervisor and stays that.

### Why two supervisors rather than one that handles both

Considered and rejected: expanding `session-mgr` to serve both. Three reasons, all of which
Linux also ran into — it has never had a single process that logs you in over serial, SSH *and*
a display.

**Governing decision 3 is the deciding one.** `display-arm-plan.md`: *"Serial keeps working
throughout … `eshell`'s recovery path and headless CI all depend on the serial console."* One
process serving both logins means a fault on the graphical path costs the serial console — the
one thing the whole display arm has promised not to break. Linux's answer is the same and for
the same reason: a crashing greeter must not take out `getty`.

**The two supervisors are not the same kind of program.** `session-mgr` presents its prompt on
a `Tty` it opens like any other client, and
[`session-mgr/CLAUDE.md`](../../userspace/session-mgr/CLAUDE.md) restricts it to `libkern` +
`librsproto` + `libstream` + `libheap` — deliberately, because it is a supervisor whose death is
a system fault. A graphical greeter is *itself a compositor client*: it needs `/dev/draw` in its
own namespace and links `libdraw` + `libui` + `libsurface`. That dependency cannot be admitted
to `session-mgr` without giving up the rule.

**Privilege separation stays available.** `gdm` runs its greeter as an unprivileged session of
its own precisely so the thing drawing on screen is not the thing holding everyone's auth path.
Two processes keep that option; one forecloses it.

### What Linux did instead, and which half applies

Linux unified its entry points **twice**, in two different ways, and only one of them is
relevant now:

- **PAM (1995)** — a shared *library*. `login`, `sshd` and every display manager link it, sharing
  the credential logic while each owns its own entry mechanism. **This is the half that
  applies**, and it is §3 below.
- **`systemd-logind` (2011)** — a central *registry* of what sessions exist, which each entry
  point notifies from inside PAM. It authenticates nobody and spawns no shell; it owns session
  bookkeeping, seats, device ACLs and idle/lock.

Nitrox does not need the registry yet — [`session-and-auth.md`](../architecture/session-and-auth.md)
defers "concurrent logins (one console, one session at a time)". It is named here so a future
multi-seat or switch-user answer has somewhere obvious to go, and so nothing built now
forecloses it. **`auth-service` is not that registry and should not become it**: it is PAM's
*verifier* and nothing more — one question, one answer, no session state (§2).

## 2. The cast

Everything below `desktop-session-mgr` already exists. Only the two right-hand rows are new.

| Component | Role | Holds | State |
|---|---|---|---|
| **auth-service** | Credential oracle: "is this password right, and who is this?" | a read handle to the user DB; **no** `BIND_NAMESPACE` | built |
| **profile-server** | Projects the **system** profile at `/bin` | the store | built |
| **fs-server** | The root filesystem | the block device | built |
| **compositor** | Pixels, surfaces, windows, input routing, focus | the framebuffer, the input stream | built |
| **session-mgr** | Serial session supervisor | `BIND_NAMESPACE`, fs/profile/tty endpoints, an auth channel | built |
| **desktop-session-mgr** | **Graphical** session supervisor | the same, **plus** a `/dev/draw` connection | **new** |
| **desktop-shell** | The graphical session's leader: bar, applications modal, window placement policy, application namespaces | its session namespace; `BIND_NAMESPACE` for the namespaces it builds | **new** |

**`auth-service`'s protocol is untouched.** Both supervisors ask it the same question over the
same protocol — `Authenticate { username, password } → { AUTHENTICATED, principal, home } |
DENIED` ([`rsproto-auth-ops.md`](../spec/rsproto-auth-ops.md)). That the graphical path needs no
change *there* is the strongest evidence the existing split was drawn in the right place:
credential validation was already separate from session lifecycle and namespace construction.

**Its plumbing was not untouched, and an earlier draft said it was — resolved by M7 Part C
(2026-08-25).** `auth-service` used to create exactly one channel pair at startup and transfer
the one client end in `Meta::Ready`, with `service-mgr` couriering it to `session-mgr`: a
single-client server, with no second endpoint for `desktop-session-mgr` to hold and no
`/svc/auth` to resolve one from. It is a **namespace forwarder** now — `init` binds its endpoint
at `/svc/auth` and it mints a session per caller — so `desktop-session-mgr` resolves its own,
and the protocol did not change. See
[`session-and-auth.md`](../architecture/session-and-auth.md) for the built shape.

The correction is kept rather than deleted because "no change here" was doing work in this
document that it could not support (details pass, 2026-08-21), and because this is the doc that
governs Part D.

**`profile-server` is also untouched, and is worth stating precisely because it is easy to
misremember**: it projects the *system* profile, identical for every user. There is no per-user
profile lookup in session construction today — per-user overlays are deferred
(`session-and-auth.md` → Deferred). A graphical session gets the same `/bin` a serial one does.

## 3. The authority chain

```
kernel ─spawns→ init (full SysCaps)
  init ─spawns, binds /svc/auth→ auth-service (no caps; a forwarder, resolved by each client)
  init ─spawns, delegates BIND_NAMESPACE→ service-mgr
    ├─spawns, re-delegates BIND_NAMESPACE→ session-mgr
    │      + fs ep, profile ep, tty ep;  auth resolved from /svc/auth
    │      └─on login→ session ns ─spawns→ nxsh
    │
    └─spawns, re-delegates BIND_NAMESPACE→ desktop-session-mgr        ← new
           + fs ep, profile ep, tty ep, auth channel, /dev/draw
           └─on login→ desktop session ns ─spawns, re-delegates
                       BIND_NAMESPACE→ desktop-shell                   ← new
                          └─per application→ app ns ─spawns→ nxterm, …
```

Every arrow attenuates. The new rows extend the existing concentration rather than widening it:
`BIND_NAMESPACE` reaches one more supervisor tier, and the reason is the same one that put it in
`session-mgr` — **the process that composes a namespace must hold the endpoint handles it binds**
([`why-supervisor-registration.md`](../rationale/why-supervisor-registration.md)).

**`desktop-shell` holding `BIND_NAMESPACE` is the one genuinely new grant, and it is load-bearing
rather than convenient.** `ui-composition-model.md` §5a states both halves: the shell "holds a
full-rights handle to every application's namespace because it *created* that namespace at
spawn", and from that — *"An application cannot compose other applications … it is structural:
no application holds a handle to another's namespace."* An application cannot construct a
namespace, so it can never reach into a peer's. That isolation is what the grant buys, and it is
why the shell has to be the process that *built* the namespaces rather than one handed them: a
shell given namespaces someone else constructed would need no `BIND_NAMESPACE` — and could not
make the guarantee either, because whoever did construct them would still hold their handles.

**It does not make the shell a fourth supervisor tier by accident.** It is the graphical session's
leader — the counterpart of `nxsh`, which likewise spawns things, but with the addition that a GUI
application needs a *constructed* namespace where a shell's child can inherit one.

### The shell also serves, and that has to be reconciled

`desktop-shell` is **both** a namespace constructor and a resource server:
[`ui-composition-model.md`](../design/ui-composition-model.md) has `/dev/desktop/` "served by the desktop
shell", and `/dev/desktop/1/windows/` as a filtered view of the compositor's window set. That
combination runs straight at two rules this document otherwise leans on, and an earlier draft cited
them as support without noticing (PR #193 review, finding 4):

- [`syscaps.md`](../architecture/syscaps.md): init delegates `BIND_NAMESPACE` to "coordination
  processes that construct namespaces" and **never** to "an ordinary resource server (which
  registers via the supervisor)".
- [`why-supervisor-registration.md`](../rationale/why-supervisor-registration.md): "Even if a
  resource server were granted `BIND_NAMESPACE` only to register itself once, the capability
  persists… Better to never grant it."

**The reconciliation is the parenthetical in the first rule.** The prohibited category is the
*ordinary* resource server, and the reason given is that it would otherwise register itself. So the
property to preserve is not "the shell must not serve" but:

> **`desktop-shell` does not bind its own endpoint.** `desktop-session-mgr` binds `/dev/desktop`
> into the session namespace, exactly as `init` binds the tty server's endpoint and `session-mgr`
> binds `/dev/tty`. The shell holds `BIND_NAMESPACE` to construct *application* namespaces —
> continuously, as its job — not to register itself once.

That keeps the RS startup protocol intact: the shell sends `Meta::Ready` with its endpoint and a
supervisor binds it, like every other server in the system.

**Two things follow, and both are consequences rather than free.** The trusted set widens — a
`BIND_NAMESPACE` holder that also draws on screen and parses input is a larger, more exposed
process than `service-mgr` or `session-mgr`, and the "lateral expansion" objection applies to it in
full. And if that ever stops being acceptable, the split is available: the serving half
(`/dev/desktop`) can move to a separate process, leaving the constructor half in the shell. Naming
that now is cheaper than discovering it is impossible later.

## 4. The session recipe, and what the two supervisors share

Both supervisors run the same five steps. Only the first and last differ.

| step | `session-mgr` | `desktop-session-mgr` |
|---|---|---|
| 1. present a login | `login:` on a `Tty` it opened, echo off via `SetMode` | a `libui` window on a compositor surface |
| 2. authenticate | `Authenticate` → auth-service | **identical** |
| 3. construct the namespace | `sys_ns_create` + binds | same recipe, different device bindings (§5) |
| 4. spawn the leader | `/bin/nxsh`, `syscaps: 0` | `/bin/desktop-shell`, `BIND_NAMESPACE` |
| 5. reap, tear down, re-present | close ns, close tty, re-prompt | close ns, destroy windows, re-present greeter |

Steps 2–4 are the same logic against different arguments, which is what
[`display-arm-plan.md`](../planning/display-arm-plan.md) Milestone 7 factors into a shared crate.
**The split follows Linux's PAM precedent** — shared library, separate supervisors — and it has a
hard constraint attached: the shared core must honour `session-mgr`'s dependency rule
(`libkern` + `librsproto` + `libstream` + `libheap`, no `libos`), because `session-mgr` links it.
The greeter — the part that draws — stays in each supervisor, which is exactly where the two
diverge anyway.

**Step 1 is not a trivial difference.** `session-mgr` opens its prompt's `Tty` the way any
program does, and closes it at session end as the revocation point. `desktop-session-mgr` must be
a compositor client *before* anyone has authenticated, which means its own namespace carries
`/dev/draw` and its greeter window exists across logins. That is closer to `gdm`'s
`class=greeter` session than to anything `session-mgr` does.

## 5. The graphical session's namespace

The serial recipe, with three deliberate differences:

| path | source | in a graphical session? |
|---|---|---|
| `/home` | fs endpoint, **subtree-scoped** to the principal's home | yes, unchanged |
| `/bin` | profile endpoint, whole tree, read-only | yes, unchanged |
| `/session/user` | a read-only memory-object snapshot of the principal | yes, unchanged |
| `/dev/draw` | the compositor's forwarding endpoint | **added** — without it the shell cannot draw |
| `/dev/console` | direct-handle bind of the console device | **omitted** — see below |
| `/dev/tty` | the tty server's forwarding endpoint | **open question** — see §6 |

**`/dev/console` is deliberately absent.** It is the serial recovery path's device, and two
sessions holding it is not a hypothetical: `display-arm-plan.md`'s governing decision 3 records
that when the tty server first held a permanent console read it *swallowed `session-mgr`'s login
input*, and it took a failing interactive test to find. A graphical session has no business
naming it.

## 6. Open questions

**6.1 — What is `/dev/tty` inside a graphical application? — ANSWERED 2026-08-25 by Milestone 7
Part F: it is the tty server's *factory*, bound uniformly.** The question assumed a terminal
emulator's `/dev/tty` "should reach its own window — which is a different binding per
application", and therefore that the answer had to be a per-application binding, an absence, or
named terminal groups. All three take `/dev/tty` to be a *name for a terminal*. It is not: it
resolves to the tty server's endpoint, which **mints** terminals. The per-application part comes
from minting and from `Tty::AttachBackend`, not from the binding.

So an application namespace binds `/dev/tty` the same way for every application
(`desktop-shell`'s `build_app_namespace`), and `nxterm` opens a terminal, attaches its own
window as that terminal's backend, and hands the terminal to `nxsh` **as a handle** — which is
why two terminals do not contend: each mints its own, on its own backend. `nxsh` takes a
handed-down terminal when its parent gives one and resolves `/dev/tty` otherwise, which is the
second shape's mechanism confirmed, without the first clause: the name stays present, because
minting is what it is for.

**One edge is left, and it is an attenuation problem rather than a naming one.** A terminal
minted *without* attaching a backend sits on the **console** backend, and `drive` gives each
completed line to the first tty there with an outstanding read — so a graphical-session program
that opened one and read could take a line the serial column's `nxsh` is waiting for. That is
the console authority §5's governing decision 3 deliberately withholds from this session,
reachable by a different route. Nothing does it today (`nxterm` attaches, `desktop-shell` never
opens a terminal), and closing it needs a mechanism to hand out a mint-only `/dev/tty` rather
than an edit here. It stays tracked as `TODO(gui-dev-tty)` in
[`deferred-decisions.md`](../rationale/deferred-decisions.md), narrowed to exactly this.

**6.2 — Are serial and graphical sessions concurrent? — ANSWERED 2026-08-21: two independent
sessions.** `session-mgr` and `desktop-session-mgr` each authenticate and run a session, unaware
of each other. Maintainer's decision during Milestone 7's details pass.

Serial staying available while a graphical session runs is then governing decision 3 holding
*by construction* rather than by care — there is nothing to arbitrate, so there is nothing that
can decide wrongly. It matches Linux, where `getty` and `gdm` do not coordinate, and it needs no
registry, which is what §1 says Nitrox does not need yet.

**The costs are real and accepted**: the same principal may hold two sessions with two
namespaces at once, and nothing arbitrates between them. The alternative — one session with two
views — is closer to the letter of `session-and-auth.md`'s deferred "one console, one session at
a time", but requires the session registry §1 explicitly defers, so it would have made the
graphical session wait on a logind-shaped component to exist. If that component ever arrives,
this is the decision to revisit; nothing built for two sessions forecloses one.

**6.3 — Does the session's process supervision live in `desktop-shell`?** GNOME separates "logs
you in" (`gdm`) from "supervises the session's processes" (`gnome-session` / `systemd --user`)
from "draws" (`gnome-shell`). This design folds the middle into `desktop-shell`. That is right
while the shell is the only thing spawning applications; it stops being right if session
services appear that must outlive or precede the shell.

**6.4 — Which process places windows before `desktop-shell` exists? — ANSWERED by Milestone 6.**
Placement is *policy*, which [`desktop-shell.md`](desktop-shell.md) §8 assigns to the shell, so
the compositor needed a default and a seam the shell could take over through. It has both: a
window lands at the origin and is held uncomposited until configured, a manager attaches through
`/dev/draw/manage` and drives `Place`/`Raise`/`Lower`/`RaiseAbove`/`SetFocus`, and with no
manager attached the hold is skipped so the screen still works. Naming it here was so it would
not be discovered in Milestone 7, and that worked: the greeter draws before any manager exists
because of the last clause.

What Milestone 6 did **not** settle is who may be the manager — `TODO(manage-ungated)`, closed in
Milestone 7 Part E.

## 7. What this does not do

- **No seats.** One screen, one keyboard, one mouse. Multi-seat is where a logind-shaped registry
  would land (§1) and nothing here precludes it.
- **No session tokens, no switch-user, no lock screen.** All deferred with the serial equivalents.
- **No per-user profile overlays.** Deferred in `session-and-auth.md`; a graphical session gets
  the system `/bin` exactly as a serial one does.
- **No SSH.** The left-hand column names it because that is where it would attach — `sshd` would
  be a third entry point sharing the same core — but nothing in Phase 4 builds it.

## References

- [`session-and-auth.md`](../architecture/session-and-auth.md) — the serial column, as built
- [`ui-composition-model.md`](../design/ui-composition-model.md) — §5a/§6, the shell's namespace authority
- [`desktop-shell.md`](desktop-shell.md) — what the shell presents
- [`display-substrate.md`](../design/display-substrate.md) — the mechanism beneath both
- [`why-supervisor-registration.md`](../rationale/why-supervisor-registration.md) — why a leaf
  never constructs its own authority
- [`display-arm-plan.md`](../planning/display-arm-plan.md) — the build order
