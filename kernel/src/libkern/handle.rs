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
///  63  62                          32 31              20 19              0
/// ┌──┬─────────────────────────────┬──────────────────┬──────────────────┐
/// │ 0│      generation counter      │   segment id     │ index in segment │
/// └──┴─────────────────────────────┴──────────────────┴──────────────────┘
///   ▲          31 bits                  12 bits             20 bits
///   └─ reserved zero
/// ```
///
/// **Bit 63 is reserved zero.** Syscalls return handles in the result
/// register, which also encodes [`KError`](crate::syscall) values as
/// *negative* `isize`. Were the generation a full 32 bits, a handle
/// whose generation reached `0x8000_0000` would set bit 63 and read
/// back as a negative `isize`, aliasing an error code. Capping the
/// generation at 31 bits keeps every issued handle a non-negative
/// `isize`, so the value/error spaces never collide.
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

    /// Width of the generation field. Bit 63 of a handle is reserved
    /// zero (see the type-level docs), so the generation occupies bits
    /// 62:32 — 31 bits, not 32.
    pub const GENERATION_BITS: u32 = 31;

    /// Largest generation a slot may be issued with. The handle table
    /// bumps a slot's generation modulo `GENERATION_MAX + 1`, so it
    /// **wraps** `GENERATION_MAX` → `0` and bit 63 stays clear. Distinct
    /// generations let a stale `RawHandle` for a recycled slot fail the
    /// generation check; the wrap admits a narrow, accepted ABA (a stale
    /// handle re-validating only after `2^31` reuses of the *same* slot,
    /// and only within the same owning process). See
    /// `docs/spec/handle-encoding.md` § "Generation counter behavior".
    pub const GENERATION_MAX: u32 = (1 << Self::GENERATION_BITS) - 1;

    /// Pack a `(segment, slot, generation)` triple into a `RawHandle`.
    ///
    /// `seg_id` must be `< 4096` (12 bits), `slot_id` must be
    /// `< 1 << 20` (20 bits), and `generation` must be
    /// `<= GENERATION_MAX` (31 bits, bit 63 reserved zero); violations
    /// trip a debug assertion. In release builds out-of-range bits
    /// silently overlap the next field, which the decode side reads
    /// back as a structurally valid but logically wrong handle that
    /// will fail the table's directory / per-segment bounds check or
    /// the generation check on first lookup.
    pub const fn encode(seg_id: u32, slot_id: u32, generation: u32) -> Self {
        debug_assert!(seg_id < 4096, "segment id overflows 12-bit field");
        debug_assert!(slot_id < (1 << 20), "slot id overflows 20-bit field");
        debug_assert!(
            generation <= Self::GENERATION_MAX,
            "generation overflows 31-bit field — bit 63 is reserved zero",
        );
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

/// User-visible handle metadata, written by `sys_handle_stat`.
///
/// `#[repr(C)]` boundary type (`docs/spec/syscall-abi.md`). Field order is
/// chosen so the `u64` sits first (no leading pad) and the two `u32`s pack
/// into the trailing 8 bytes — total 16 bytes, 8-byte aligned, no interior
/// padding (asserted below). `owner_pid` is intentionally omitted: a process
/// can only stat handles it owns (the table's lookup enforces
/// `owner_pid == caller`), so reporting it back is redundant.
///
/// Not part of the kernel ABI version hash (`docs/spec/abi-version-hash.md`
/// lists the hashed types; `HandleInfo` is not among them).
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct HandleInfo {
    /// The handle's current rights bitmask (`Rights::bits()`).
    pub rights: u64,
    /// The referenced object's [`KObjectType`] discriminant (`as u32`).
    pub object_type: u32,
    /// The handle's generation counter (bits 62:32 of the [`RawHandle`];
    /// bit 63 is reserved zero).
    pub generation: u32,
}

const _: () = assert!(core::mem::size_of::<HandleInfo>() == 16);
const _: () = assert!(core::mem::align_of::<HandleInfo>() == 8);
const _: () = assert!(core::mem::offset_of!(HandleInfo, rights) == 0);
const _: () = assert!(core::mem::offset_of!(HandleInfo, object_type) == 8);
const _: () = assert!(core::mem::offset_of!(HandleInfo, generation) == 12);

impl HandleInfo {
    /// Build the user-facing info from a handle's metadata snapshot. Takes
    /// primitives (not the table's `HandleStat`) so this type keeps its
    /// no-allocator, no-table dependency and can be shared with userspace.
    pub const fn from_stat(object_type: KObjectType, rights: Rights, generation: u32) -> Self {
        Self {
            rights: rights.bits(),
            object_type: object_type as u32,
            generation,
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
        // Generation tops out at `GENERATION_MAX` (31 bits); bit 63 of the
        // handle is reserved zero, so `u32::MAX`-style generations are no
        // longer representable (and trip the encode assertion).
        for (seg, slot, generation) in [
            (0u32, 0u32, 0u32),
            (1, 1, 1),
            (4095, (1 << 20) - 1, RawHandle::GENERATION_MAX),
            (42, 12345, 0x4AFE_F00D),
            (1, 0, RawHandle::GENERATION_MAX),
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
    fn issued_handles_are_non_negative_isize() {
        // Bit 63 is reserved zero, so even a max-generation handle on the
        // last segment/slot stays a non-negative `isize` and can never be
        // mistaken for a `KError` in the syscall result register.
        assert_eq!(RawHandle::GENERATION_MAX, 0x7FFF_FFFF);
        let h = RawHandle::encode(4095, (1 << 20) - 1, RawHandle::GENERATION_MAX);
        assert_eq!(h.bits() >> 63, 0, "bit 63 must be reserved zero");
        assert!((h.bits() as i64) >= 0);
    }

    #[test]
    #[should_panic]
    fn encode_rejects_generation_above_max() {
        // A generation past `GENERATION_MAX` would set the reserved bit 63;
        // debug builds catch it.
        let _ = RawHandle::encode(0, 0, RawHandle::GENERATION_MAX + 1);
    }

    #[test]
    fn encoded_bit_positions_match_spec() {
        // generation occupies bits 62..32 (bit 63 reserved zero); segment
        // occupies 31..20; slot occupies 19..0.
        let generation = 0x5EAD_BEEF; // <= GENERATION_MAX, bit 31 clear
        let h = RawHandle::encode(0xABC, 0x12345, generation);
        let raw = h.bits();
        assert_eq!(raw >> 63, 0, "bit 63 is reserved zero");
        assert_eq!((raw >> 32) as u32, generation, "generation lives in bits 62:32");
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

    // --- HandleInfo ---------------------------------------------------

    #[test]
    fn handle_info_from_stat_maps_fields() {
        let rights = Rights::DUPLICATE | Rights::INSPECT | Rights::SIGNAL;
        let info = HandleInfo::from_stat(KObjectType::Process, rights, 0xDEAD_BEEF);
        assert_eq!(info.rights, rights.bits());
        assert_eq!(info.object_type, KObjectType::Process as u32);
        assert_eq!(info.generation, 0xDEAD_BEEF);
    }

    #[test]
    fn handle_info_layout_is_stable() {
        // Mirrors the compile-time asserts; documents the wire layout.
        assert_eq!(core::mem::size_of::<HandleInfo>(), 16);
        assert_eq!(core::mem::align_of::<HandleInfo>(), 8);
        assert_eq!(core::mem::offset_of!(HandleInfo, rights), 0);
        assert_eq!(core::mem::offset_of!(HandleInfo, object_type), 8);
        assert_eq!(core::mem::offset_of!(HandleInfo, generation), 12);
    }
}
