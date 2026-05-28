//! ABI-level value types for the kernel handle system.
//!
//! [`RawHandle`], [`Rights`], and [`KObjectType`] are the wire-format
//! primitives the kernel and (eventually) userspace must agree on.
//! `docs/spec/handle-encoding.md` is the normative source; this module
//! is its in-kernel embodiment. The handle *table* — the segmented
//! directory that maps handles to objects — lives in
//! `kernel/src/handle/`; this file holds only the pure value types so
//! they have no allocator or arch dependency and can be shared with
//! userspace later without dragging the table along.
//!
//! All operations on these types are `const`-callable and allocation-
//! free. Bitflags are hand-rolled in the [`PageFlags`](crate::arch::paging::PageFlags)
//! style (see `kernel/CLAUDE.md` — no `bitflags` crate).

use core::ops::{BitAnd, BitOr, BitOrAssign};

/// A 64-bit opaque capability identifier.
///
/// Bit layout, per `docs/spec/handle-encoding.md`:
///
/// ```text
///  63                              32 31              20 19              0
/// ┌────────────────────────────────┬──────────────────┬──────────────────┐
/// │       generation counter       │   segment id     │ index in segment │
/// └────────────────────────────────┴──────────────────┴──────────────────┘
///        u32                          12 bits             20 bits
/// ```
///
/// [`RawHandle::NULL`] is reserved and never issued by the kernel; it
/// is the canonical "no handle" sentinel for userspace and the kernel's
/// own bookkeeping fields (`HandleEntry::next_owned` when the slot is
/// at the tail of its owner's list).
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub struct RawHandle(pub u64);

impl RawHandle {
    /// The reserved invalid handle. Never issued by the kernel; safe
    /// for userspace to use as a "no handle" placeholder.
    pub const NULL: RawHandle = RawHandle(0);

    /// Pack a `(segment, slot, generation)` triple into a `RawHandle`.
    ///
    /// `seg_id` must be `< 4096` (12 bits) and `slot_id` must be
    /// `< 1 << 20` (20 bits); violations trip a debug assertion. In
    /// release builds out-of-range bits silently overlap the next
    /// field, which the decode side reads back as a structurally
    /// valid but logically wrong handle that will fail the table's
    /// directory or per-segment bounds check on first lookup.
    pub const fn encode(seg_id: u32, slot_id: u32, generation: u32) -> Self {
        debug_assert!(seg_id < 4096, "segment id overflows 12-bit field");
        debug_assert!(slot_id < (1 << 20), "slot id overflows 20-bit field");
        let slot = ((seg_id as u64) << 20) | (slot_id as u64);
        Self(((generation as u64) << 32) | slot)
    }

    /// Unpack into `(segment, slot, generation)`. Inverse of [`encode`].
    ///
    /// [`encode`]: RawHandle::encode
    pub const fn decode(self) -> (u32, u32, u32) {
        let slot = self.0 as u32;
        let seg_id = slot >> 20;
        let slot_id = slot & ((1 << 20) - 1);
        let generation = (self.0 >> 32) as u32;
        (seg_id, slot_id, generation)
    }

    /// `true` iff this is [`RawHandle::NULL`].
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The raw 64-bit value, for tests, debugging, and the syscall ABI
    /// crossing.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

/// A set of capability bits attached to a handle.
///
/// Hand-rolled bitflags following the [`PageFlags`](crate::arch::paging::PageFlags)
/// pattern. The constants and bit positions are normative — see
/// `docs/spec/handle-encoding.md` § "Rights bitmask".
///
/// Three bands:
///
/// - **Generic** (bits 0-3): apply to any handle type.
/// - **Principal** (bits 8-19): per-type "what can this handle *do*".
/// - **Modifier** (bits 32-37): per-type "and how" (`SEEK`,
///   `APPEND`, ...).
///
/// Subset semantics live on this type: `r1.is_subset_of(r2)` iff
/// `(r1 & r2) == r1`. The handle table never amplifies rights.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Rights(u64);

impl Rights {
    // --- Generic rights (apply to all handle types) --------------------
    /// `sys_handle_duplicate` is permitted.
    pub const DUPLICATE: Rights = Rights(1 << 0);
    /// Handle may be transferred across an IPC boundary.
    pub const TRANSFER: Rights = Rights(1 << 1);
    /// Handle metadata is readable via `sys_handle_stat`.
    pub const INSPECT: Rights = Rights(1 << 2);
    /// `sys_wait` accepts this handle.
    pub const WAIT: Rights = Rights(1 << 3);

    // --- Principal rights (type-specific) ------------------------------
    /// I/O read.
    pub const READ: Rights = Rights(1 << 8);
    /// I/O write.
    pub const WRITE: Rights = Rights(1 << 9);
    /// Execute (mapped memory).
    pub const EXECUTE: Rights = Rights(1 << 10);
    /// Send an out-of-band signal (`Process`, `Thread`).
    pub const SIGNAL: Rights = Rights(1 << 11);
    /// Terminate (`Process`, `Thread`).
    pub const TERMINATE: Rights = Rights(1 << 12);
    /// Resolve names within a namespace.
    pub const LOOKUP: Rights = Rights(1 << 13);
    /// Bind a name into a namespace.
    pub const BIND: Rights = Rights(1 << 14);
    /// Map readable.
    pub const MAP_READ: Rights = Rights(1 << 15);
    /// Map writable.
    pub const MAP_WRITE: Rights = Rights(1 << 16);
    /// Map executable.
    pub const MAP_EXEC: Rights = Rights(1 << 17);
    /// IPC send.
    pub const SEND: Rights = Rights(1 << 18);
    /// IPC receive.
    pub const RECV: Rights = Rights(1 << 19);

    // --- Modifier rights -----------------------------------------------
    /// Seek within a stream resource.
    pub const SEEK: Rights = Rights(1 << 32);
    /// Append-only write.
    pub const APPEND: Rights = Rights(1 << 33);
    /// Truncate a resource to zero.
    pub const TRUNCATE: Rights = Rights(1 << 34);
    /// Remove an existing namespace binding.
    pub const UNBIND: Rights = Rights(1 << 35);
    /// Enumerate a namespace's bindings.
    pub const ENUMERATE: Rights = Rights(1 << 36);
    /// Inspect a process's memory map.
    pub const INSPECT_MEMORY: Rights = Rights(1 << 37);

    /// The empty set.
    pub const fn empty() -> Self {
        Rights(0)
    }

    /// Wrap a raw bit pattern. The caller is responsible for ensuring
    /// only documented bits are set; unknown bits are accepted but
    /// never matched by any rights query, so they survive
    /// transfer/duplicate untouched and have no operational meaning.
    pub const fn from_bits_truncate(bits: u64) -> Self {
        Rights(bits)
    }

    /// The raw bit pattern, for tests, debugging, and the syscall ABI
    /// crossing.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// `true` if every bit set in `other` is also set in `self`.
    pub const fn contains(self, other: Rights) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Bitwise union.
    pub const fn union(self, other: Rights) -> Self {
        Rights(self.0 | other.0)
    }

    /// Bitwise intersection.
    pub const fn intersection(self, other: Rights) -> Self {
        Rights(self.0 & other.0)
    }

    /// `true` iff `self ⊆ other`, i.e. every bit set in `self` is also
    /// set in `other`. The spec's normative definition of "is a subset
    /// of".
    pub const fn is_subset_of(self, other: Rights) -> bool {
        (self.0 & other.0) == self.0
    }
}

impl BitOr for Rights {
    type Output = Rights;
    fn bitor(self, rhs: Rights) -> Rights {
        self.union(rhs)
    }
}

impl BitAnd for Rights {
    type Output = Rights;
    fn bitand(self, rhs: Rights) -> Rights {
        self.intersection(rhs)
    }
}

impl BitOrAssign for Rights {
    fn bitor_assign(&mut self, rhs: Rights) {
        self.0 |= rhs.0;
    }
}

/// Discriminant identifying which kind of kernel object a handle
/// refers to.
///
/// Per `kernel/CLAUDE.md` § "Kernel object dispatch": kernel objects
/// are dispatched via `match` on this enum rather than via `dyn Trait`,
/// which keeps `HandleEntry::object` an 8-byte `AtomicPtr<()>` rather
/// than a 16-byte fat pointer and keeps the entry cache-line sized.
///
/// Discriminants are fixed by the spec's type-rights compatibility
/// matrix (`docs/spec/handle-encoding.md`) and contribute to the
/// kernel ABI version hash (`docs/spec/abi-version-hash.md`); never
/// renumber or reorder them.
///
/// Phase 1 stub: only `Process` and `Thread` will have real kernel
/// implementations in the next slice; the rest are declared so the
/// type-rights matrix compiles and discriminants are stable. They land
/// behind their respective slices in later Phase 1 work.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KObjectType {
    /// Reserved; never appears in a live entry.
    Invalid = 0,
    Process = 1,
    Thread = 2,
    Namespace = 3,
    MemoryObject = 4,
    IpcChannel = 5,
    NotificationChannel = 6,
    Timer = 7,
    InterruptObject = 8,
    PendingOperation = 9,
    IoRing = 10,
    EntropyObject = 11,
    DeviceNode = 12,
    ResourceServerReg = 13,
}

impl KObjectType {
    /// Decode a u32 discriminant back into a `KObjectType`, or `None`
    /// if the value is not one of the declared variants. The handle
    /// table stores the discriminant in an `AtomicU32` for lock-free
    /// snapshot reads; this helper recovers the enum on the way out.
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Invalid),
            1 => Some(Self::Process),
            2 => Some(Self::Thread),
            3 => Some(Self::Namespace),
            4 => Some(Self::MemoryObject),
            5 => Some(Self::IpcChannel),
            6 => Some(Self::NotificationChannel),
            7 => Some(Self::Timer),
            8 => Some(Self::InterruptObject),
            9 => Some(Self::PendingOperation),
            10 => Some(Self::IoRing),
            11 => Some(Self::EntropyObject),
            12 => Some(Self::DeviceNode),
            13 => Some(Self::ResourceServerReg),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- RawHandle ----------------------------------------------------

    #[test]
    fn null_is_zero_and_decodes_trivially() {
        assert_eq!(RawHandle::NULL.bits(), 0);
        assert!(RawHandle::NULL.is_null());
        assert_eq!(RawHandle::NULL.decode(), (0, 0, 0));
    }

    #[test]
    fn encode_decode_round_trip_at_field_corners() {
        for (seg, slot, generation) in [
            (0u32, 0u32, 0u32),
            (1, 1, 1),
            (4095, (1 << 20) - 1, u32::MAX),
            (42, 12345, 0xCAFEF00D),
            (1, 0, u32::MAX),
            (0, (1 << 20) - 1, 0),
        ] {
            let h = RawHandle::encode(seg, slot, generation);
            assert_eq!(
                h.decode(),
                (seg, slot, generation),
                "round trip for {seg:?},{slot:?},{generation:?}",
            );
        }
    }

    #[test]
    fn encoded_bit_positions_match_spec() {
        // generation occupies bits 63..32; segment occupies 31..20;
        // slot occupies 19..0.
        let h = RawHandle::encode(0xABC, 0x12345, 0xDEAD_BEEF);
        let raw = h.bits();
        assert_eq!((raw >> 32) as u32, 0xDEAD_BEEF, "generation lives in bits 63:32");
        assert_eq!(((raw as u32) >> 20) & 0xFFF, 0xABC, "segment id lives in bits 31:20");
        assert_eq!((raw as u32) & ((1 << 20) - 1), 0x12345, "slot id lives in bits 19:0");
    }

    #[test]
    fn nonzero_encoded_handle_is_not_null() {
        let h = RawHandle::encode(0, 1, 0);
        assert!(!h.is_null());
    }

    // --- Rights -------------------------------------------------------

    #[test]
    fn empty_has_no_bits() {
        assert_eq!(Rights::empty().bits(), 0);
        assert!(Rights::empty().is_subset_of(Rights::READ | Rights::WRITE));
    }

    #[test]
    fn union_and_intersection_combine_bits() {
        let rw = Rights::READ | Rights::WRITE;
        assert!(rw.contains(Rights::READ));
        assert!(rw.contains(Rights::WRITE));
        assert!(!rw.contains(Rights::EXECUTE));
        let r = rw & Rights::READ;
        assert_eq!(r.bits(), Rights::READ.bits());
    }

    #[test]
    fn subset_semantics_per_spec() {
        let r = Rights::READ;
        let rw = Rights::READ | Rights::WRITE;
        // r ⊆ rw
        assert!(r.is_subset_of(rw));
        // rw ⊄ r — write is set in rw but not in r
        assert!(!rw.is_subset_of(r));
        // The empty set is a subset of everything.
        assert!(Rights::empty().is_subset_of(rw));
        // Everything is a subset of itself.
        assert!(rw.is_subset_of(rw));
    }

    #[test]
    fn or_assign_accumulates() {
        let mut r = Rights::READ;
        r |= Rights::WRITE;
        assert!(r.contains(Rights::READ));
        assert!(r.contains(Rights::WRITE));
    }

    #[test]
    fn modifier_bits_occupy_high_half() {
        // SEEK is bit 32; assert it lands above the 32-bit boundary.
        assert_eq!(Rights::SEEK.bits(), 1 << 32);
        assert_eq!(Rights::INSPECT_MEMORY.bits(), 1 << 37);
    }

    #[test]
    fn from_bits_truncate_preserves_pattern() {
        let bits = (Rights::READ | Rights::WRITE | Rights::SEEK).bits();
        let r = Rights::from_bits_truncate(bits);
        assert_eq!(r.bits(), bits);
    }

    // --- KObjectType --------------------------------------------------

    #[test]
    fn discriminants_match_spec() {
        assert_eq!(KObjectType::Invalid as u32, 0);
        assert_eq!(KObjectType::Process as u32, 1);
        assert_eq!(KObjectType::Thread as u32, 2);
        assert_eq!(KObjectType::IoRing as u32, 10);
        assert_eq!(KObjectType::ResourceServerReg as u32, 13);
    }

    #[test]
    fn kobject_type_is_four_bytes() {
        assert_eq!(core::mem::size_of::<KObjectType>(), 4);
        assert_eq!(core::mem::align_of::<KObjectType>(), 4);
    }
}
