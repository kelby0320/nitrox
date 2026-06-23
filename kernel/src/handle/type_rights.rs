//! Allocation-time check that a [`Rights`] set is compatible with a
//! given [`KObjectType`].
//!
//! Implements the type-rights compatibility matrix from
//! `docs/spec/handle-encoding.md`. The kernel rejects nonsensical
//! right combinations (e.g. `MAP_WRITE` on a `Process` handle) at
//! allocation time so the lookup hot path never has to second-guess
//! the matrix.
//!
//! ## What is checked
//!
//! - **Principal rights** (bits 8-19): must be a subset of the
//!   per-type mask below.
//! - **Generic rights** (bits 0-3) and **modifier rights** (bits
//!   32-37): not checked here. The spec calls generic rights "valid
//!   on any handle type" and is silent on modifier-rights validity
//!   per type; modifier-rights enforcement, where required, lands
//!   with the slice that introduces each affected type.

use crate::libkern::handle::{KObjectType, Rights};

/// Mask covering every principal-rights bit (8-19) defined by the
/// spec. Used to isolate the principal-rights portion of a requested
/// `Rights` set before comparing against a type-specific mask.
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

/// Principal rights valid on [`KObjectType::Process`] handles.
const PROCESS_PRINCIPALS: Rights =
    Rights::from_bits_truncate(Rights::SIGNAL.bits() | Rights::TERMINATE.bits());

/// Principal rights valid on [`KObjectType::Thread`] handles.
const THREAD_PRINCIPALS: Rights =
    Rights::from_bits_truncate(Rights::SIGNAL.bits() | Rights::TERMINATE.bits());

/// Principal rights valid on [`KObjectType::Namespace`] handles.
const NAMESPACE_PRINCIPALS: Rights =
    Rights::from_bits_truncate(Rights::LOOKUP.bits() | Rights::BIND.bits());

/// Principal rights valid on [`KObjectType::MemoryObject`] handles.
const MEMORY_OBJECT_PRINCIPALS: Rights = Rights::from_bits_truncate(
    Rights::MAP_READ.bits() | Rights::MAP_WRITE.bits() | Rights::MAP_EXEC.bits(),
);

/// Principal rights valid on [`KObjectType::IpcChannel`] handles.
const IPC_CHANNEL_PRINCIPALS: Rights =
    Rights::from_bits_truncate(Rights::SEND.bits() | Rights::RECV.bits());

/// Principal rights valid on [`KObjectType::IoRing`] handles. The
/// SQE/CQE memory regions need read and write for the producer and
/// consumer halves of the ring.
const IO_RING_PRINCIPALS: Rights =
    Rights::from_bits_truncate(Rights::READ.bits() | Rights::WRITE.bits());

/// Principal rights valid on [`KObjectType::EntropyObject`] handles.
const ENTROPY_PRINCIPALS: Rights = Rights::READ;

/// Principal rights valid on [`KObjectType::DeviceNode`] handles.
/// `INSPECT` is a generic right; it has no place on the principal mask.
const DEVICE_NODE_PRINCIPALS: Rights = Rights::READ;

/// Wait-only types — [`KObjectType::NotificationChannel`],
/// [`KObjectType::Timer`], [`KObjectType::InterruptObject`],
/// [`KObjectType::PendingOperation`]. `WAIT` is a generic right
/// (bit 3), not a principal one, so the principal mask is empty —
/// allocations may carry `WAIT` via the generic-rights band.
const WAIT_ONLY_PRINCIPALS: Rights = Rights::empty();

/// `true` iff `rights` is a valid set for a handle of type `ty`.
///
/// Generic and modifier rights are accepted without check; only
/// principal rights (bits 8-19) are matched against the spec's
/// per-type allowlist. `KObjectType::Invalid` and
/// `KObjectType::UserspaceServerReg` are rejected outright — neither
/// is a user-accessible type.
pub(crate) fn is_rights_compatible(ty: KObjectType, rights: Rights) -> bool {
    let allowed_principal = match ty {
        KObjectType::Process => PROCESS_PRINCIPALS,
        KObjectType::Thread => THREAD_PRINCIPALS,
        KObjectType::Namespace => NAMESPACE_PRINCIPALS,
        KObjectType::MemoryObject => MEMORY_OBJECT_PRINCIPALS,
        KObjectType::IpcChannel => IPC_CHANNEL_PRINCIPALS,
        KObjectType::NotificationChannel
        | KObjectType::Timer
        | KObjectType::InterruptObject
        | KObjectType::PendingOperation => WAIT_ONLY_PRINCIPALS,
        KObjectType::IoRing => IO_RING_PRINCIPALS,
        KObjectType::EntropyObject => ENTROPY_PRINCIPALS,
        KObjectType::DeviceNode => DEVICE_NODE_PRINCIPALS,
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
        ] {
            assert!(is_rights_compatible(ty, Rights::empty()), "{:?}", ty);
        }
    }

    #[test]
    fn invalid_and_userspace_server_reg_always_rejected() {
        assert!(!is_rights_compatible(KObjectType::Invalid, Rights::empty()));
        assert!(!is_rights_compatible(
            KObjectType::UserspaceServerReg,
            Rights::empty(),
        ));
    }

    #[test]
    fn process_accepts_signal_terminate_only() {
        assert!(is_rights_compatible(KObjectType::Process, Rights::SIGNAL));
        assert!(is_rights_compatible(KObjectType::Process, Rights::TERMINATE));
        assert!(is_rights_compatible(
            KObjectType::Process,
            Rights::SIGNAL | Rights::TERMINATE,
        ));
        // READ is a principal right not in Process's mask.
        assert!(!is_rights_compatible(KObjectType::Process, Rights::READ));
        // MAP_WRITE is principal — the spec's own example of a
        // nonsensical combination.
        assert!(!is_rights_compatible(KObjectType::Process, Rights::MAP_WRITE));
    }

    #[test]
    fn memory_object_accepts_map_band() {
        let map_all = Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC;
        assert!(is_rights_compatible(KObjectType::MemoryObject, map_all));
        // But not plain READ/WRITE — those are for streams/rings.
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
    fn wait_only_types_accept_generic_wait_but_no_principal_rights() {
        for ty in [
            KObjectType::NotificationChannel,
            KObjectType::Timer,
            KObjectType::InterruptObject,
            KObjectType::PendingOperation,
        ] {
            // WAIT is generic (bit 3), not principal — accepted.
            assert!(is_rights_compatible(ty, Rights::WAIT), "{:?}", ty);
            // READ is principal — rejected.
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
        // SEEK / APPEND / TRUNCATE / INSPECT_MEMORY are modifier bits.
        // The current matrix doesn't restrict them per type; document
        // that behaviour with a positive test.
        let with_modifier = Rights::SIGNAL | Rights::SEEK | Rights::INSPECT_MEMORY;
        assert!(is_rights_compatible(KObjectType::Process, with_modifier));
    }
}
