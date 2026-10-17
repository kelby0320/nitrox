//! Allocation-time check that a [`Rights`] set is compatible with a given
//! [`KObjectType`], per the matrix in `docs/spec/handle-encoding.md`: nonsensical
//! combinations (e.g. `MAP_WRITE` on a `Process`) are rejected at allocation, so
//! the lookup hot path never second-guesses it.
//!
//! Only **principal rights** (bits 8-19) are checked, as a subset of the
//! per-type mask. **Generic** (0-3) and **modifier** (32-37) rights are not: the
//! spec calls generic rights valid on any type and is silent on modifier rights
//! per type.

use crate::libkern::handle::{KObjectType, Rights};

/// Every principal-rights bit (8-19), to isolate that portion of a request.
const PRINCIPAL_MASK: Rights = Rights::from_bits_truncate(
    Rights::READ.bits()
        | Rights::WRITE.bits()
        | Rights::EXECUTE.bits()
        | Rights::SIGNAL.bits()
        | Rights::TERMINATE.bits()
        | Rights::LOOKUP.bits()
        | Rights::BIND.bits()
        | Rights::MAP_READ.bits()
        | Rights::MAP_WRITE.bits()
        | Rights::MAP_EXEC.bits()
        | Rights::SEND.bits()
        | Rights::RECV.bits(),
);

/// Principal rights valid per object type.
const PROCESS_PRINCIPALS: Rights = Rights::from_bits_truncate(Rights::SIGNAL.bits() | Rights::TERMINATE.bits());

const THREAD_PRINCIPALS: Rights = Rights::from_bits_truncate(Rights::SIGNAL.bits() | Rights::TERMINATE.bits());

const NAMESPACE_PRINCIPALS: Rights = Rights::from_bits_truncate(Rights::LOOKUP.bits() | Rights::BIND.bits());

const MEMORY_OBJECT_PRINCIPALS: Rights =
    Rights::from_bits_truncate(Rights::MAP_READ.bits() | Rights::MAP_WRITE.bits() | Rights::MAP_EXEC.bits());

/// Principal rights valid on [`KObjectType::FileObject`] handles: mappable like
/// a `MemoryObject` (the `MAP_*` band). The actual grant is attenuated at
/// resolve time; `MAP_WRITE` is in the type mask for read-write mappings.
const FILE_OBJECT_PRINCIPALS: Rights = MEMORY_OBJECT_PRINCIPALS;

const IPC_CHANNEL_PRINCIPALS: Rights = Rights::from_bits_truncate(Rights::SEND.bits() | Rights::RECV.bits());

/// Principal rights valid on [`KObjectType::IoRing`] handles: read and write,
/// for the producer and consumer halves of the SQE/CQE regions.
const IO_RING_PRINCIPALS: Rights = Rights::from_bits_truncate(Rights::READ.bits() | Rights::WRITE.bits());

const ENTROPY_PRINCIPALS: Rights = Rights::READ;

/// Principal rights valid on [`KObjectType::DeviceNode`] handles: `READ` and
/// `WRITE` (a block device is written via `sys_io_submit`). `INSPECT` is a
/// generic right, not on the principal mask.
const DEVICE_NODE_PRINCIPALS: Rights = Rights::READ.union(Rights::WRITE);

/// Principal rights valid on [`KObjectType::SystemControl`] handles: `WRITE`,
/// which `sys_power` requires.
const SYSTEM_CONTROL_PRINCIPALS: Rights = Rights::WRITE;

/// Wait-only types ([`KObjectType::NotificationChannel`], `Timer`,
/// `InterruptObject`, `PendingOperation`). `WAIT` is a generic right, so the
/// principal mask is empty.
const WAIT_ONLY_PRINCIPALS: Rights = Rights::empty();

/// `true` iff `rights` is valid for a handle of type `ty`: only principal
/// rights are matched against the per-type allowlist. `Invalid` and
/// `UserspaceServerReg` are rejected outright, being no user-accessible type.
pub(crate) fn is_rights_compatible(ty: KObjectType, rights: Rights) -> bool {
    let allowed_principal = match ty {
        KObjectType::Process => PROCESS_PRINCIPALS,
        KObjectType::Thread => THREAD_PRINCIPALS,
        KObjectType::Namespace => NAMESPACE_PRINCIPALS,
        KObjectType::MemoryObject => MEMORY_OBJECT_PRINCIPALS,
        KObjectType::FileObject => FILE_OBJECT_PRINCIPALS,
        KObjectType::IpcChannel => IPC_CHANNEL_PRINCIPALS,
        KObjectType::NotificationChannel
        | KObjectType::Timer
        | KObjectType::InterruptObject
        | KObjectType::PendingOperation => WAIT_ONLY_PRINCIPALS,
        KObjectType::IoRing => IO_RING_PRINCIPALS,
        KObjectType::EntropyObject => ENTROPY_PRINCIPALS,
        KObjectType::DeviceNode => DEVICE_NODE_PRINCIPALS,
        KObjectType::SystemControl => SYSTEM_CONTROL_PRINCIPALS,
        KObjectType::Invalid | KObjectType::UserspaceServerReg => return false,
    };
    let requested_principal = rights & PRINCIPAL_MASK;
    requested_principal.is_subset_of(allowed_principal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rights_accepted_for_any_real_type() {
        for ty in [
            KObjectType::Process,
            KObjectType::Thread,
            KObjectType::Namespace,
            KObjectType::MemoryObject,
            KObjectType::IpcChannel,
            KObjectType::IoRing,
            KObjectType::Timer,
            KObjectType::EntropyObject,
            KObjectType::DeviceNode,
            KObjectType::SystemControl,
        ] {
            assert!(is_rights_compatible(ty, Rights::empty()), "{:?}", ty);
        }
    }

    #[test]
    fn invalid_and_userspace_server_reg_always_rejected() {
        assert!(!is_rights_compatible(KObjectType::Invalid, Rights::empty()));
        assert!(!is_rights_compatible(KObjectType::UserspaceServerReg, Rights::empty(),));
    }

    #[test]
    fn process_accepts_signal_terminate_only() {
        assert!(is_rights_compatible(KObjectType::Process, Rights::SIGNAL));
        assert!(is_rights_compatible(KObjectType::Process, Rights::TERMINATE));
        assert!(is_rights_compatible(KObjectType::Process, Rights::SIGNAL | Rights::TERMINATE,));
        assert!(!is_rights_compatible(KObjectType::Process, Rights::READ));
        assert!(!is_rights_compatible(KObjectType::Process, Rights::MAP_WRITE));
    }

    #[test]
    fn memory_object_accepts_map_band() {
        let map_all = Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC;
        assert!(is_rights_compatible(KObjectType::MemoryObject, map_all));
        assert!(!is_rights_compatible(KObjectType::MemoryObject, Rights::READ));
    }

    #[test]
    fn ipc_channel_accepts_send_recv() {
        assert!(is_rights_compatible(KObjectType::IpcChannel, Rights::SEND));
        assert!(is_rights_compatible(KObjectType::IpcChannel, Rights::RECV));
        assert!(!is_rights_compatible(KObjectType::IpcChannel, Rights::READ));
    }

    #[test]
    fn io_ring_accepts_read_write() {
        let rw = Rights::READ | Rights::WRITE;
        assert!(is_rights_compatible(KObjectType::IoRing, rw));
        assert!(!is_rights_compatible(KObjectType::IoRing, Rights::EXECUTE));
    }

    #[test]
    fn system_control_accepts_write_only() {
        assert!(is_rights_compatible(KObjectType::SystemControl, Rights::WRITE | Rights::INSPECT));
        assert!(!is_rights_compatible(KObjectType::SystemControl, Rights::READ));
        assert!(!is_rights_compatible(KObjectType::SystemControl, Rights::BIND));
    }

    #[test]
    fn wait_only_types_accept_generic_wait_but_no_principal_rights() {
        for ty in [
            KObjectType::NotificationChannel,
            KObjectType::Timer,
            KObjectType::InterruptObject,
            KObjectType::PendingOperation,
        ] {
            assert!(is_rights_compatible(ty, Rights::WAIT), "{:?}", ty);
            assert!(!is_rights_compatible(ty, Rights::READ), "{:?}", ty);
        }
    }

    #[test]
    fn generic_rights_accepted_alongside_valid_principal_rights() {
        let combo = Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT | Rights::SIGNAL;
        assert!(is_rights_compatible(KObjectType::Process, combo));
    }

    #[test]
    fn modifier_rights_are_not_checked() {
        // Modifier bits are not restricted per type.
        let with_modifier = Rights::SIGNAL | Rights::SEEK | Rights::INSPECT_MEMORY;
        assert!(is_rights_compatible(KObjectType::Process, with_modifier));
    }
}
