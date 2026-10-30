//! In-kernel resource servers and their dispatch registry.
//!
//! A **Kernel Server** is an in-kernel function `sys_ns_lookup` calls, in the caller's syscall
//! context, to produce a handle for a
//! [`BindingTarget::KernelServer`](crate::object::namespace::BindingTarget) binding; the other
//! kind is [`super::userspace_server`] (`docs/architecture/namespace-and-resource-servers.md`
//! § "Kernel Servers"). The syscall parks the rights-attenuated object and pre-signals the
//! lookup's `PendingOperation`, so an in-kernel lookup is synchronous.

use crate::libkern::KBox;
use crate::libkern::handle::{KObjectType, Rights};
use crate::object::DeviceNode;
use crate::object::EntropyObject;
use crate::object::MemoryObject;
use crate::object::ObjectRef;
use crate::object::Process;
use crate::syscall::error::KError;

/// Identifies one in-kernel resource server; `Copy`, so a `KernelServer` binding needs no
/// outside-the-lock drop.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KernelServerId {
    /// `/dev/entropy` — the kernel CSPRNG (see [`entropy_server`]).
    Entropy,
    /// `/proc/self/process` — the caller's own [`Process`] (see [`proc_self_process`]).
    ProcSelfProcess,
    /// `/proc/self/thread` — the calling [`Thread`](crate::object::Thread).
    ProcSelfThread,
    /// `/proc/self/namespace` — the caller's root [`Namespace`](crate::object::Namespace).
    ProcSelfNamespace,
    /// `/proc/self/status` — the caller's pid/tid as a read-only [`MemoryObject`] text snapshot.
    ProcSelfStatus,
    /// `/proc/cmdline` — the command line this boot was given, as text.
    ProcCmdline,
    /// `/initramfs/<path>` — a boot CPIO file, as a read-only [`MemoryObject`] copy.
    Initramfs,
    /// `/dev/input/raw/<n>` — the `n`-th raw input device (`0` keyboard, `1` mouse).
    RawInput,
    /// `/dev/blk/<n>` — the `n`-th block device; one binding owns the subtree.
    BlockDevice,
    /// `/dev/console` — the serial console, a char [`DeviceNode`](crate::object::DeviceNode).
    Console,
    /// `/dev/log` — the kernel log ring as a read-only [`MemoryObject`] snapshot.
    Log,
    /// `/proc/sched/stats` — per-CPU scheduler statistics as a read-only [`MemoryObject`].
    SchedStats,
    /// `/dev/framebuffer` — the display aperture, the one server with **two leaves**.
    Framebuffer,
    /// `/dev/registry` — the device table; **root namespace only**, served onward by the device
    /// manager at `/dev/devices`.
    Registry,
}

/// The outcome of a resource-server lookup. A Kernel Server answers synchronously, so it
/// returns only [`Completed`](OpStatus::Completed) or [`Rejected`](OpStatus::Rejected);
/// `Pending` belongs to the userspace path.
pub enum OpStatus {
    /// A kernel object; the caller pre-signals the lookup with it, rights-attenuated.
    Completed(ObjectRef),
    /// The lookup failed; the caller delivers `err` through the lookup PO.
    Rejected(KError),
    /// Forwarded to a Userspace Server: the lookup's `PendingOperation` stays **uncompleted**.
    Pending,
}

/// Call the in-kernel server `id` with the lookup `suffix` (past the binding prefix, no
/// leading `/`; empty on an exact match), in the caller's syscall context.
///
/// Rights *attenuation* is the lookup syscall's job (`requested ∩ binding.rights`): a server
/// returns a full-rights object and ignores `requested`.
pub fn dispatch(id: KernelServerId, suffix: &[u8], requested: Rights) -> OpStatus {
    match id {
        KernelServerId::Entropy => entropy_server(suffix, requested),
        KernelServerId::ProcSelfProcess => proc_self_process(suffix, requested),
        KernelServerId::ProcSelfThread => proc_self_thread(suffix, requested),
        KernelServerId::ProcSelfNamespace => proc_self_namespace(suffix, requested),
        KernelServerId::ProcSelfStatus => proc_self_status(suffix, requested),
        KernelServerId::ProcCmdline => proc_cmdline(suffix, requested),
        KernelServerId::Initramfs => initramfs_server(suffix, requested),
        KernelServerId::BlockDevice => block_device_server(suffix, requested),
        KernelServerId::RawInput => raw_input_server(suffix, requested),
        KernelServerId::Console => console_server(suffix, requested),
        KernelServerId::Log => log_server(suffix, requested),
        KernelServerId::Framebuffer => framebuffer_server(suffix, requested),
        KernelServerId::SchedStats => sched_stats_server(suffix, requested),
        KernelServerId::Registry => registry_server(suffix, requested),
    }
}

/// Adopt a fresh [`MemoryObject`] into a [`Completed`](OpStatus::Completed) answer.
fn complete_with_memobj(obj: KBox<MemoryObject>) -> OpStatus {
    // SAFETY: `into_raw` yields the one outstanding creation reference.
    OpStatus::Completed(unsafe {
        ObjectRef::from_raw(KBox::into_raw(obj).as_ptr() as *mut (), KObjectType::MemoryObject)
    })
}

/// `/dev/entropy` — a **leaf**; an exact match mints an [`EntropyObject`] on the global CSPRNG.
fn entropy_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match EntropyObject::try_new() {
        // SAFETY: `into_raw` yields the one outstanding creation reference.
        Ok(obj) => OpStatus::Completed(unsafe {
            ObjectRef::from_raw(KBox::into_raw(obj).as_ptr() as *mut (), KObjectType::EntropyObject)
        }),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

// --- /proc/self — self-reference servers (no ambient authority) ---
//
// **Leaves** returning the **caller's own** object from the running syscall context: no pid
// parameter to forge, reachable only if a supervisor bound it
// (`docs/architecture/namespace-and-resource-servers.md` § "`/proc/self`"). `None` (a boot
// thread with no process) → `NotFound`.

/// `/proc/self/process` — the caller's own [`Process`] handle.
fn proc_self_process(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match crate::sched::current_process() {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/proc/self/thread` — the calling [`Thread`](crate::object::Thread) handle.
fn proc_self_thread(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match crate::sched::current_thread() {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/proc/self/namespace` — the caller's root [`Namespace`](crate::object::Namespace).
fn proc_self_namespace(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    let ns = crate::sched::current_process().and_then(|p| {
        // SAFETY: `current_process` returns a live `Process` ObjectRef; `p` pins it
        // for this borrow. `namespace_ref` clones the stored namespace ObjectRef.
        unsafe { &*(p.as_ptr() as *const Process) }.namespace_ref()
    });
    match ns {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/proc/cmdline` — the boot command line as a read-only [`MemoryObject`]. A leaf, served
/// whole, so a word meant for userspace needs no kernel-side list to survive the journey.
fn proc_cmdline(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match MemoryObject::try_new_filled(crate::cmdline::line().as_bytes()) {
        Ok(obj) => complete_with_memobj(obj),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

/// `/proc/self/status` — a **leaf**: the caller's own `pid=`/`tid=` as a [`MemoryObject`]
/// snapshot, captured under one `SCHED` hold ([`crate::sched::current_pid_tid`]).
fn proc_self_status(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    let Some((pid, tid)) = crate::sched::current_pid_tid() else {
        return OpStatus::Rejected(KError::NotFound);
    };
    let text = match crate::kformat!("pid={pid}\ntid={tid}\n") {
        Ok(t) => t,
        Err(_) => return OpStatus::Rejected(KError::OutOfMemory),
    };
    match MemoryObject::try_new_filled(text.as_bytes()) {
        Ok(obj) => complete_with_memobj(obj),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

/// `/initramfs/<path>` — a **subtree** server: a read-only [`MemoryObject`] copy of a boot file.
fn initramfs_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    let Some(blob) = crate::initramfs::blob() else {
        return OpStatus::Rejected(KError::NotFound);
    };
    let Some(data) = crate::initramfs::lookup(blob, suffix) else {
        return OpStatus::Rejected(KError::NotFound);
    };
    match MemoryObject::try_new_filled(data) {
        Ok(obj) => complete_with_memobj(obj),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

/// `/dev/blk/<n>` — a **subtree** server: a decimal index resolves to the `n`-th block
/// [`DeviceNode`](crate::object::DeviceNode); `<n>/info` yields its `BlockDeviceInfo`.
fn block_device_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    // `<n>/info` says what `<n>`: without it a program knows only an index, and this registry
    // mixes whole disks, partitions and memory published as a disk.
    let (index_bytes, want_info) = match suffix.strip_suffix(b"/info") {
        Some(head) => (head, true),
        None => (suffix, false),
    };
    let Some(index) = parse_index(index_bytes) else {
        return OpStatus::Rejected(KError::NotFound);
    };
    let Some(node) = crate::device::find_block_device(index) else {
        return OpStatus::Rejected(KError::NotFound);
    };
    if !want_info {
        return OpStatus::Completed(node);
    }
    // SAFETY: `find_block_device` returns a live `DeviceNode` reference.
    let info = unsafe { &*(node.as_ptr() as *const DeviceNode) }.block_info();
    match MemoryObject::try_new_filled(info.as_bytes()) {
        Ok(obj) => complete_with_memobj(obj),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

/// `/dev/input/raw/<n>` — the `n`-th raw input device, a char
/// [`DeviceNode`](crate::object::DeviceNode).
///
/// **This binding is the authority to read one device unfiltered**, meant for the
/// `input-server` alone (`docs/architecture/input-subsystem.md` §5): an exclusivity that
/// binds whoever binds it, not this code.
fn raw_input_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    let Some(index) = parse_index(suffix) else {
        return OpStatus::Rejected(KError::NotFound);
    };
    match crate::device::find_input_device(index) {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/dev/registry` — the device table, the one way to read it outside the kernel.
///
/// - `""` → a read-only [`MemoryObject`]: a `RegistryHeader` whose **count** is the number of
///   `DeviceRecord`s (`crate::libkern::device`) that follow, zero-padded to the page.
/// - `"<id>"` → node `id`, so the device manager can hand a class owner its device.
///   **`NotFound` for a departed device**: its record stays, its node is no longer handed out.
/// - `"changes"` → the change node: a `Read` waits until the table's generation is past the
///   read's `offset`, then answers with the generation.
/// - `"io"` → a read-only [`MemoryObject`]: an `IoHeader` of the page cache's counters, then
///   an `IoRecord` per disk and RAM disk.
///
/// **Authority is the binding**, made in the root namespace only: `<id>` is every raw device.
fn registry_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if suffix.is_empty() {
        let bytes = match crate::device::registry_snapshot() {
            Ok(b) => b,
            Err(_) => return OpStatus::Rejected(KError::OutOfMemory),
        };
        return match MemoryObject::try_new_filled(&bytes) {
            Ok(obj) => complete_with_memobj(obj),
            Err(_) => OpStatus::Rejected(KError::OutOfMemory),
        };
    }
    if suffix == b"io" {
        let bytes = match crate::device::io_snapshot() {
            Ok(b) => b,
            Err(_) => return OpStatus::Rejected(KError::OutOfMemory),
        };
        return match MemoryObject::try_new_filled(&bytes) {
            Ok(obj) => complete_with_memobj(obj),
            Err(_) => OpStatus::Rejected(KError::OutOfMemory),
        };
    }
    if suffix == b"changes" {
        return match crate::device::changes_node() {
            Some(node) => OpStatus::Completed(node),
            None => OpStatus::Rejected(KError::NotFound),
        };
    }
    let Some(id) = parse_index(suffix) else {
        return OpStatus::Rejected(KError::NotFound);
    };
    match crate::device::node(id as u32) {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/dev/log` — a **leaf**: the kernel log ring as a read-only [`MemoryObject`] snapshot
/// (`dmesg`), the last page zero-padded.
fn log_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    let len = crate::klog::len();
    let memobj = match MemoryObject::try_new(len.max(1)) {
        Ok(m) => m,
        Err(_) => return OpStatus::Rejected(KError::OutOfMemory),
    };
    crate::klog::copy_into_frames(memobj.frames());
    complete_with_memobj(memobj)
}

/// `/dev/framebuffer` — the display aperture, with **two leaves**.
///
/// - `""` → a **borrowed** [`MemoryObject`] over the firmware aperture. Borrowed is
///   load-bearing: the frames are MMIO, and an owned object would hand them to the buddy
///   allocator when the last handle closed (see
///   [`FrameOwnership`](crate::object::memory_object::FrameOwnership)).
/// - `"info"` → a read-only [`MemoryObject`] of one
///   [`FramebufferInfo`](crate::libkern::framebuffer::FramebufferInfo): two leaves rather than
///   a richer `HandleInfo`, a frozen layout shared by every type
///   (`docs/design/display-substrate.md` §3).
///
/// **Authority is the binding**: a process can drive the display iff its namespace contains
/// this path. `Unsupported` when no aperture was recorded at boot, rather than a zero-sized
/// object a client would map and then scribble past.
fn framebuffer_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    let Some((phys_base, info, caching)) = crate::framebuffer::aperture() else {
        return OpStatus::Rejected(KError::Unsupported);
    };

    match suffix {
        b"" => {
            // SAFETY: `phys_base .. + byte_len` is the firmware-reported display
            // aperture, recorded once at boot. It is device memory outside every region
            // the buddy allocator manages (Limine reports it as framebuffer, not usable
            // RAM), and it stays mapped for the life of the system, its base page-aligned.
            match unsafe {
                MemoryObject::try_new_borrowed(
                    phys_base,
                    info.byte_len as usize,
                    // What the aperture said it is, recorded once at boot.
                    caching,
                )
            } {
                Ok(obj) => {
                    // **Said out loud, because nothing else can see it**: the object a client
                    // maps is the only place the aperture's answer reaches userspace, and a
                    // handout that quietly minted an ordinary object would cost 45x on the
                    // laptop with every gate still green. It reads **`obj`**, not the value
                    // passed to the constructor, so a control minting an ordinary object cannot
                    // still print "write-combining".
                    crate::kprintln!("framebuffer: handed out as {} memory", obj.caching().name());
                    // **The screen changes hands here**, before the handle exists: the console
                    // stops painting under its own lock, so nothing it draws can land on a
                    // frame this client has started. Only on success: a refused handout leaves
                    // the console drawing.
                    crate::fbcon::yield_to_userspace();
                    complete_with_memobj(obj)
                }
                Err(_) => OpStatus::Rejected(KError::OutOfMemory),
            }
        }
        b"info" => match MemoryObject::try_new_filled(info.as_bytes()) {
            Ok(obj) => complete_with_memobj(obj),
            Err(_) => OpStatus::Rejected(KError::OutOfMemory),
        },
        _ => OpStatus::Rejected(KError::NotFound),
    }
}

/// `/proc/sched/stats` — a **leaf**: per-CPU scheduler statistics as a read-only
/// [`MemoryObject`] text snapshot, captured under one `SCHED` hold
/// ([`crate::sched::stats_snapshot`]) and formatted with no lock held.
fn sched_stats_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    let snap = crate::sched::stats_snapshot();
    let text = match crate::sched::stats::format(&snap) {
        Ok(t) => t,
        Err(_) => return OpStatus::Rejected(KError::OutOfMemory),
    };
    match MemoryObject::try_new_filled(text.as_bytes()) {
        Ok(obj) => complete_with_memobj(obj),
        Err(_) => OpStatus::Rejected(KError::OutOfMemory),
    }
}

/// `/dev/console` — a **leaf**: the serial console, a char
/// [`DeviceNode`](crate::object::DeviceNode); *not found* until the console is up.
fn console_server(suffix: &[u8], _requested: Rights) -> OpStatus {
    if !suffix.is_empty() {
        return OpStatus::Rejected(KError::NotFound);
    }
    match crate::drivers::console::device_ref() {
        Some(obj) => OpStatus::Completed(obj),
        None => OpStatus::Rejected(KError::NotFound),
    }
}

/// Parse `s` as a non-empty run of ASCII decimal digits, or `None` (empty, non-digit, overflow).
fn parse_index(s: &[u8]) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    let mut n: usize = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn parse_index_accepts_decimal_rejects_junk() {
        assert_eq!(parse_index(b"0"), Some(0));
        assert_eq!(parse_index(b"7"), Some(7));
        assert_eq!(parse_index(b"42"), Some(42));
        assert_eq!(parse_index(b""), None); // empty (the /dev/blk exact match)
        assert_eq!(parse_index(b"1a"), None); // trailing junk
        assert_eq!(parse_index(b"sda"), None); // non-numeric
        assert_eq!(parse_index(b"0/sub"), None); // a deeper path
    }

    #[test]
    fn entropy_exact_match_yields_entropy_object() {
        init_global_heap();
        match dispatch(KernelServerId::Entropy, b"", Rights::READ) {
            OpStatus::Completed(obj) => {
                assert_eq!(obj.object_type(), KObjectType::EntropyObject);
                drop(obj);
            }
            OpStatus::Rejected(e) => panic!("expected Completed, got Rejected({e:?})"),
            OpStatus::Pending => panic!("a kernel server never returns Pending"),
        }
    }

    #[test]
    fn entropy_rejects_non_empty_suffix() {
        init_global_heap();
        match dispatch(KernelServerId::Entropy, b"sub", Rights::READ) {
            OpStatus::Rejected(KError::NotFound) => {}
            OpStatus::Rejected(e) => panic!("expected NotFound, got {e:?}"),
            OpStatus::Completed(_) => panic!("a non-empty suffix must not resolve on a leaf"),
            OpStatus::Pending => panic!("a kernel server never returns Pending"),
        }
    }

    // Host tests never online a CPU, so the snapshot is all offline and the text exactly the
    // header; the populated rendering is covered in `sched::tests` and the QEMU selftest.
    #[test]
    fn sched_stats_exact_match_yields_text_snapshot_memobj() {
        use crate::mm::{PAGE_SIZE, heap};
        use crate::object::MemoryObject;

        init_global_heap();
        match dispatch(KernelServerId::SchedStats, b"", Rights::MAP_READ) {
            OpStatus::Completed(obj) => {
                assert_eq!(obj.object_type(), KObjectType::MemoryObject);
                // SAFETY: `obj` pins a live MemoryObject just created above.
                let m = unsafe { &*(obj.as_ptr() as *const MemoryObject) };
                assert_eq!(m.size(), PAGE_SIZE); // one page holds the header
                let expected = b"cpus_online=0\nreaper_closed=0\nexit_closed=0\n";
                let base = (m.frames()[0].as_u64() + heap::hhdm_offset()) as *const u8;
                for (i, &want) in expected.iter().enumerate() {
                    // SAFETY: a live MemoryObject's frame is HHDM-reachable;
                    // read-only check within the page.
                    assert_eq!(unsafe { *base.add(i) }, want, "byte {i}");
                }
                // SAFETY: as above; the fill zero-pads past the text.
                assert_eq!(unsafe { *base.add(expected.len()) }, 0);
                drop(obj);
            }
            OpStatus::Rejected(e) => panic!("expected Completed, got Rejected({e:?})"),
            OpStatus::Pending => panic!("a kernel server never returns Pending"),
        }
    }

    #[test]
    fn sched_stats_rejects_non_empty_suffix() {
        init_global_heap();
        match dispatch(KernelServerId::SchedStats, b"sub", Rights::MAP_READ) {
            OpStatus::Rejected(KError::NotFound) => {}
            OpStatus::Rejected(e) => panic!("expected NotFound, got {e:?}"),
            OpStatus::Completed(_) => panic!("a non-empty suffix must not resolve on a leaf"),
            OpStatus::Pending => panic!("a kernel server never returns Pending"),
        }
    }

    // The `/proc/self/*` leaves reject a non-empty suffix before any scheduler access; their
    // success arms need a running syscall context and are covered by the QEMU selftest.
    #[test]
    fn proc_self_leaves_reject_non_empty_suffix() {
        for id in [
            KernelServerId::ProcSelfProcess,
            KernelServerId::ProcSelfThread,
            KernelServerId::ProcSelfNamespace,
            KernelServerId::ProcSelfStatus,
            KernelServerId::ProcCmdline,
        ] {
            match dispatch(id, b"sub", Rights::empty()) {
                OpStatus::Rejected(KError::NotFound) => {}
                OpStatus::Rejected(e) => panic!("{id:?}: expected NotFound, got {e:?}"),
                OpStatus::Completed(_) => panic!("{id:?}: a non-empty suffix must not resolve"),
                OpStatus::Pending => panic!("{id:?}: a kernel server never returns Pending"),
            }
        }
    }
}
