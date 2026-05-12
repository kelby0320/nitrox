# Why Capabilities

This document explains why Nitrox uses capability-based access control rather than the access-control-list model familiar from Unix and most descendants. It describes the model, the alternatives that were considered, and the reasoning behind the choice.

## The question

When a process tries to perform an operation on a system resource, how does the kernel decide whether to permit it? The Unix answer is: the calling process has an identity (UID, GID), the resource has metadata describing who is allowed to do what (mode bits, ACLs), and the kernel checks the calling process's identity against the resource's metadata at the moment of access. The Nitrox answer is fundamentally different.

## The Unix model and why it falls short

In Unix, authority is **ambient**. Every process runs as a user. Every operation the process performs is implicitly attributed to that user. Whether the process is allowed to do a thing is a function of who the user is and what permissions the resource has.

This model has well-known failure modes:

**Confused deputy problems.** A privileged process (the "deputy") performs operations on behalf of less-privileged callers. Because the deputy's authority is ambient, it cannot easily distinguish "I'm doing this on my own behalf" from "I'm doing this because Alice asked me to." Unix mitigates with `setuid`/`setgid` bits and explicit credential dropping, but the underlying issue — that authority isn't tied to specific delegations — persists.

**Coarse-grained delegation.** If you want to give a program access to a single file, you have to either give it your full identity (`setuid` to you) or arrange for the file to be readable by some group the program belongs to. Neither is principle-of-least-authority. The granularity of "user" is far coarser than the granularity at which authorization should ideally be expressed.

**Sandboxing is bolted on.** Containers, chroot, namespaces, seccomp, AppArmor, SELinux — these all exist because the basic Unix model isn't restrictive enough. They work by adding policy layers that can deny what the basic model would allow. But they don't fix the underlying issue; they paper over it.

**ACLs become unmanageable at scale.** When you start adding fine-grained permissions, the ACL grows. Real systems end up with thousands of ACL entries that nobody fully understands. The resource doesn't know who legitimately needs access; it just accumulates entries that someone, at some point, thought were necessary.

The deeper problem: in the ACL model, **the resource is the security principal**. The resource decides who can do what to it. But the resource has no idea who legitimately needs access — that's a property of the calling code, not the resource. So you end up with permissions that are either too tight (legitimate users blocked) or too loose (excess access granted defensively).

## The capability model

In a capability system, **the calling process is the security principal**. A process's authority is exactly the set of capabilities it holds — opaque tokens issued by the kernel, each representing the right to perform specific operations on a specific resource.

You don't have a "user identity" that grants ambient authority. You have a set of handles. To do something, you need a handle that allows it. To give another process the ability to do something, you transfer it a handle (or a copy with appropriate restrictions).

The defining properties:

**Authority is held, not assumed.** A process can only do what its handles permit. There is no parallel mechanism (like UID) that lets it bypass the handle check. If you don't have the handle, you can't do the thing — full stop.

**Authority is transferable and attenuable.** You can give another process a handle (or a copy of a handle with reduced rights). The recipient gets exactly the authority you gave them, no more. Delegation is precise.

**Authority is inspectable.** The kernel knows exactly who holds what handles. Auditing "who can do X?" is a database query, not a guessing game across a tangle of permission rules.

**The confused deputy problem dissolves.** When Alice asks the privileged deputy to do something on her behalf, she passes the deputy a handle representing her authority for that specific operation. The deputy uses that handle. There's no ambient authority for the deputy to accidentally use on its own behalf instead of Alice's.

## The Nitrox-specific design

Nitrox combines the capability model with **per-process namespaces** to get a property neither model has alone: sandboxing by absence rather than by denial.

In a pure capability system, you can still in principle ask "what is the file at path X?" and the system needs to answer "you don't have access" if you don't. In Nitrox, the namespace itself is per-process. A sandboxed process's namespace doesn't contain `/etc/shadow` at all. It's not "permission denied" — it's "no such path." The sandboxed process can't even articulate the question.

This is the "you find resources by name; you access them by capability" framing from the architecture overview. The two ideas work together: namespace decides what you can talk about, capability decides what you can do with the things you've found.

The handle is the join. A successful namespace lookup produces a handle. The handle's rights determine what operations are permitted. Different processes may look up the same logical resource through different namespace paths and receive handles with different rights. The same fs-server-ext4 process serves the underlying filesystem to everyone, but Alice's namespace binds `/home` to a subtree handle scoped to her directory with read-write rights, while Bob's namespace binds `/home` to a subtree handle scoped to his directory. They cannot reach each other's files because they cannot construct paths that resolve there — not because the filesystem refuses, but because their namespaces don't bind such paths.

## Practical security properties

**The owner check, not cryptographic unforgeability, is the security guarantee.** A handle is a structured 64-bit integer encoding a slot index and a generation counter. A process that correctly guesses a valid handle for a slot it doesn't own would still be rejected — the kernel checks `owner_pid` against the calling process on every lookup. Cryptographic handle values would be a defense-in-depth measure, but they're not the foundational guarantee.

**Defense in depth via randomized slot allocation.** The handle table's free list is shuffled periodically. A process that legitimately holds handle for slot 42 cannot infer that slot 43 is likely to exist or contain anything sensitive. Combined with 32-bit generations and the owner check, brute-forcing handles is computationally pointless.

**Capability transfer is kernel-mediated.** A process can't fabricate a handle and try to pass it to another process — the kernel manages all transfer atomically and validates that the source has `TRANSFER` rights. The receiving process receives a handle that's already valid in its handle table; there's no point at which an unverified handle exists.

**Attenuation is a fundamental operation.** When transferring a handle, the source can specify that the destination receives only a subset of the source's rights. A process granted a `ReadWrite` file handle can pass it to a logging service as `ReadOnly`. The logging service, regardless of how it's implemented or compromised, cannot upgrade. Attenuation matches how delegation works in well-designed systems.

## What the capability model gives up

Nothing comes free.

**No ambient authority means no convenient "I'm root, just do it" escape.** If init didn't grant a process a particular capability, that process cannot acquire it later through any in-band mechanism. To run an admin operation, you go through the privilege broker — an explicit subsystem that authenticates the request and constructs a new namespace with admin resources. There is no `sudo` that elevates an existing process; you spawn a new process with elevated handles.

**Delegation requires explicit handle transfer.** In Unix, you give a program access to a file by making the file readable. In Nitrox, you give it a handle. This is a behavioral change for shell-style ad-hoc work — the shell and runtime libraries provide ergonomic affordances, but the underlying model is more explicit.

**The kernel does more bookkeeping.** The handle table is per-system, not per-process; it has to scale to millions of entries. The reference counting, the rights checking, the transfer protocol — all of these are kernel work that ACL-based systems don't do.

These are real costs. The question is whether they're worth paying, and the answer is that they're paying for security properties that ACL-based systems can only approximate through layering.

## Influences and prior art

**seL4** is the most rigorous capability-based kernel. It's formally verified, partly because the capability model is amenable to formal reasoning in ways the Unix model is not. Nitrox is not a formal-methods project, but seL4 establishes that the capability model is sound and has been demonstrated to work in practice for serious systems.

**Fuchsia / Zircon** is the most practically successful capability-based OS at scale. Fuchsia's "everything is a capability" approach, kernel-mediated handle transfer, and namespace-as-security-boundary architecture directly inform Nitrox's design. Where Nitrox differs from Fuchsia is mostly in the userspace ecosystem (typed structured streams, content-addressed store, the specific shell model) and in some kernel choices (segmented handle table vs. Zircon's per-process tables, eager vs. lazy FPU save, etc.).

**Plan 9** influenced the per-process namespace concept, although Plan 9 doesn't have a clean capability model — it relies on conventional Unix-style permissions for access control within the namespace. Nitrox takes Plan 9's namespace ideas and combines them with proper capabilities.

**KeyKOS, EROS, Coyotos** are the lineage of capability-based research operating systems. They produced much of the theoretical groundwork (and the term "capability" itself, dating back to Dennis & Van Horn's 1966 paper). Their influence is indirect but pervasive.

## Where to read more

- [Handle system architecture](../architecture/handle-system.md) — how handles are implemented in the kernel
- [Namespace and resource servers architecture](../architecture/namespace-and-resource-servers.md) — how the namespace layer works
- [Why supervisor-mediated RS registration](why-supervisor-registration.md) — a specific application of capability discipline
- [Rejected approaches](rejected-approaches.md) — alternatives considered and the reasons they were not chosen
