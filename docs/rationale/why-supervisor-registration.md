# Why Supervisor-Mediated Resource Server Registration

Resource servers in Nitrox do not register themselves. A resource server creates an endpoint, hands the endpoint handle to a supervisor (init or service manager) over a control IPC channel, and the supervisor — holding `SysCaps::BIND_NAMESPACE` — calls `sys_ns_bind` to register the endpoint at a chosen path in the namespace. This document explains why.

## The natural-seeming alternative

The intuitive design is: a resource server holds `SysCaps::BIND_NAMESPACE`, calls `sys_ns_bind` on itself with the appropriate path and rights, and is now registered. This is one fewer process involved, one fewer IPC handshake, one less moving part. Many systems work this way.

The reason Nitrox doesn't is **principle of least authority**. A resource server's only legitimate use of `BIND_NAMESPACE` is to register itself once at startup. After that, the capability is dead weight in its hands. A bug or compromise in the resource server could in principle bind additional entries into the system namespace — entries the resource server has no business creating. Impersonating other resource servers, shadowing legitimate bindings, planting confused-deputy traps. The capability is far more powerful than the resource server's legitimate need for it.

`BIND_NAMESPACE` is a high-trust capability. It lets the holder define what `/proc`, `/dev`, `/store`, `/home`, and every other namespace path resolves to. A process with `BIND_NAMESPACE` can effectively redefine the system from a naming perspective. This is appropriate for init, service manager, and session manager — they are coordination processes whose job includes namespace construction. It is inappropriate for an fs-server whose job is reading and writing ext4 blocks.

## The supervisor-mediated protocol

The startup protocol for a resource server goes:

1. **Supervisor spawns the resource server** with the resources it needs to operate (block device handle, log channel, minimal own-namespace) plus a **control IPC channel** between supervisor and resource server. The supervisor does not grant `BIND_NAMESPACE` to the resource server.

2. **Resource server initializes** — reads superblock for fs-server, reads device configuration for a driver-backed RS, sets up its internal data structures.

3. **Resource server creates an endpoint** — an IPC channel end that the kernel will route lookup/submit requests to once this RS is bound into a namespace. Internally this means the RS calls `sys_channel_create` and retains the receive end; the send end becomes the endpoint handle the kernel uses.

4. **Resource server signals "Ready"** on the control channel, including the endpoint handle in the message.

5. **Supervisor receives the Ready message** and the endpoint handle.

6. **Supervisor calls `sys_ns_bind(target_namespace, path, endpoint, rights)`** to register the endpoint as the resource server for `path` in the target namespace, with the rights the supervisor chooses to grant.

7. **The RS is live.** Lookups resolving to `path` are routed by the kernel to the endpoint; the resource server receives the requests on its retained receive end.

The control channel is not discarded after registration. It remains the management channel between supervisor and resource server for ongoing lifecycle:

- Supervisor sends: shutdown, reload, health-check, configuration update requests
- Resource server sends: error notifications, degraded-state signals, statistics

This is structurally similar to how systemd manages services — there's both the service's function (handling requests) and the supervisor's control over the service (lifecycle management). The supervisor-mediated registration is one specific use of the control channel; ongoing management is the rest.

## What this prevents

**Compromised RS binding malicious entries.** If an attacker compromises fs-server, they can read and write the filesystem (which is fs-server's legitimate authority). They cannot bind additional entries into the system namespace, redirect `/proc` to a fake server, shadow `/dev/log` with a logger that drops messages, or perform any other namespace-level mischief. Their authority is bounded by fs-server's actual job.

**Unintentional self-registration mistakes.** A resource server that holds `BIND_NAMESPACE` could, through a bug, bind itself at the wrong path or with the wrong rights. The supervisor, holding the cap, makes one explicit decision per RS based on configuration. The mistake surface is smaller and the decisions are concentrated where they can be reviewed.

**Lateral expansion.** Even if a resource server were granted `BIND_NAMESPACE` only to register itself once, the capability persists. Attenuating it to "single-use" or "scoped to one path" is awkward — single-use capabilities aren't a Nitrox concept, and a path-scoped `BIND_NAMESPACE` for the RS's own path is essentially the cap itself. Better to never grant it.

## Why a single-use attenuated capability isn't worth pursuing

A natural counter-proposal: grant the resource server a `BIND_NAMESPACE` capability that's attenuated to a specific path and consumed on first use. The RS uses it to bind itself at startup; thereafter the capability is gone.

This was considered and rejected because:

1. **Single-use capabilities aren't an existing Nitrox concept.** Adding them just for this one case is overengineering. The capability model is rich enough already; introducing a new semantic (consume-on-use) for one caller is bad value.

2. **Attenuation to a single path doesn't restrict meaningfully.** The whole point of binding at `/store` is owning that subtree's naming. A `BIND_NAMESPACE` cap restricted to the path the RS will register at gives the RS exactly the authority that's at issue.

3. **The supervisor pattern is already there.** The control channel exists for ongoing lifecycle management regardless. Folding the registration handshake into that channel adds essentially nothing — one extra message at startup. The supervisor was going to be in the protocol anyway.

4. **Concentration of `BIND_NAMESPACE` is a desirable property in itself.** Having a small set of supervisory processes that own all namespace mutation gives a chokepoint where namespace policy can be enforced and audited. This is structural; it's worth preserving even if the per-RS savings of not using it were larger than they are.

## Who actually holds `BIND_NAMESPACE`

At system start:

- **Init** holds the full set of system capabilities, including `BIND_NAMESPACE`. Init is the initial coordinator; it builds the system namespace by binding the in-kernel resource servers and the first userspace resource servers (rootfs fs-server, store profile server).

- **Service manager** holds delegated `BIND_NAMESPACE` for the subtrees it manages. Init delegates a scoped cap to service manager so service manager can bind subsequent services without going back to init for each.

- **Session manager** holds delegated `BIND_NAMESPACE` for per-session subtrees (user home subtrees, per-session tmp namespaces, per-session profile bindings). Service manager delegates this when it spawns session manager.

That's the entire list at system start. Every other process — every fs-server, every netstack-server, every device driver, every profile server, every user application — does not hold `BIND_NAMESPACE`. The capability is concentrated in three coordination roles.

This is consistent with the broader capability discipline: capabilities flow from supervisor to subordinate, attenuated as appropriate, and never amplified. `BIND_NAMESPACE` is a powerful capability that controls how the namespace is constructed; it lives where namespace construction is the job.

## Operational consequences

**Resource servers can be swapped in place.** If init wants to upgrade fs-server to a new version, it spawns the new fs-server, waits for Ready, binds the new endpoint (possibly unbinding the old), and signals the old fs-server to shut down. The bound endpoints are kernel-side handles; switching them is an init operation, not an RS operation.

**Resource servers don't need namespace metadata.** Because the RS doesn't choose its own binding location, it doesn't need to know "I am the fs-server for /store." It just serves whatever the kernel sends to its endpoint. The same fs-server-ext4 binary can be spawned multiple times for multiple block devices and bound at multiple paths; no per-instance configuration in the RS's own knowledge.

**Audit is straightforward.** Every namespace binding in the system was created by init, service manager, or session manager. Auditing "who put this binding here?" is a small set of suspects. Logging in those three processes captures the entire namespace-mutation story.

## Where to read more

- [Resource Server Model architecture](../architecture/namespace-and-resource-servers.md) — full RS model including the registration protocol
- [Why capabilities](why-capabilities.md) — the broader principle this is an application of
- [Service.toml schema](../spec/service-toml-schema.md) — how service declarations specify the supervisor relationship
