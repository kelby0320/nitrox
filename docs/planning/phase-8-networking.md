# Nitrox Implementation Plan — Phase 8 — Networking

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. This phase is
**planned, not built** — nothing below describes current behaviour.

---

## Phase 8: Networking

**Goal:** a Nitrox program fetches something over HTTPS.

**Agreed 2026-08-25 as "Phase 6"** and renumbered (decision log, 2026-09-10). It was kept
separate from [the portable runtime](phase-7-portable-runtime.md) deliberately: networking is a
driver-and-protocol project and the runtime is a loader-and-ABI project, and interleaving them
in one plan would mean two half-finished things for longer.

### Tasks

- [ ] **A network driver.** virtio-net for the QEMU loop; **RTL8111/8168** for the target
      laptop. Two drivers is the point rather than a cost — a second one is what shows whether
      the device interface generalises, the same way a second input producer does in
      [Phase 6](phase-6-usb.md).
- [ ] **A userspace netstack server** — smoltcp ported, or from scratch. Userspace for the same
      reason filesystems are: it is a resource server, not kernel business.
- [ ] **Socket-as-namespace-resource** — the architectural question of this phase. A socket is
      a capability obtained from a server, not a number in a per-process table, and the design
      has to say what a listening socket, an accepted connection and a datagram endpoint each
      are as namespace objects.
- [ ] **DHCP and DNS** as ordinary userspace clients.
- [ ] **TLS** via `rustls` plus a Rust crypto provider. `libcrypto` already has SHA-256 / HMAC
      / PBKDF2 hand-rolled; this is a much larger surface and is a dependency question, not a
      write-it-ourselves question.

### Definition of Done

An HTTPS fetch from a userspace program on real hardware, over the laptop's own Ethernet.

### What Phase 8 does not do

Wireless (the target machine's Wi-Fi is a separate driver and a separate supplicant project),
IPv6 beyond what the stack gives for free, a firewall, or NAT.
