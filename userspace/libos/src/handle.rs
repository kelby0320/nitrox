//! `Handle<T, M>` — the typed capability handle.
//!
//! Implements the typestate design from `docs/history/os-design-v5.1.md`: `T` is the
//! object-type marker, `M` the mode marker encoding **principal** rights as a type
//! (so the wrong operation is a compile error), and `extra` carries the generic +
//! modifier rights checked at runtime. A `Handle` **owns** its capability and closes
//! it on drop (RAII); [`into_raw`](Handle::into_raw) extracts it without closing (for
//! transfer). Object methods live in [`crate::objects`].

use core::marker::PhantomData;

use libkern::{RawHandle, Rights};

use crate::error::{Error, Result};
use crate::sys;

// --- object-type markers (T) ----------------------------------------------

/// A `MemoryObject` — anonymous mappable memory.
pub struct Memory;
/// A `Namespace` — a capability-scoped path→resource map.
pub struct Namespace;
/// A `NotificationChannel` — the async-event queue.
pub struct Notify;
/// A readable/writable resource (device or file) driven by `io_submit`.
pub struct Resource;
/// A child `Process` (from [`spawn`](crate::spawn)); reaped by closing the handle.
pub struct Process;
/// A `Thread` in this process (from [`thread_create`](crate::thread_create)).
pub struct Thread;

// --- mode markers (M) — principal rights as types -------------------------

/// Memory mapped read-only.
pub struct MapRead;
/// Memory mapped read+write.
pub struct MapReadWrite;
/// Memory mapped read+execute.
pub struct MapExec;
/// Namespace that can only be looked up in.
pub struct NsReadOnly;
/// Namespace that can also be bound into.
pub struct NsMutable;
/// Resource opened read-only.
pub struct ReadOnly;
/// Resource opened write-only.
pub struct WriteOnly;
/// Resource opened read+write.
pub struct ReadWrite;
/// The sole mode for objects with no principal-rights variation (e.g. `Notify`).
pub struct Only;

// --- sealed capability traits (op gating) ---------------------------------

mod sealed {
    pub trait Sealed {}
}
use sealed::Sealed;

macro_rules! seal {
    ($($m:ty),+ $(,)?) => { $( impl Sealed for $m {} )+ };
}
seal!(MapRead, MapReadWrite, MapExec, NsReadOnly, NsMutable, ReadOnly, WriteOnly, ReadWrite, Only);

/// A resource mode that permits `read` (device → buffer).
pub trait CanRead: Sealed {}
impl CanRead for ReadOnly {}
impl CanRead for ReadWrite {}

/// A resource mode that permits `write` (buffer → device).
pub trait CanWrite: Sealed {}
impl CanWrite for WriteOnly {}
impl CanWrite for ReadWrite {}

/// A memory mode whose mapping is readable — a valid **write**-op buffer (the
/// kernel reads the buffer).
pub trait CanMapRead: Sealed {}
impl CanMapRead for MapRead {}
impl CanMapRead for MapReadWrite {}

/// A memory mode whose mapping is writable — a valid **read**-op buffer (the kernel
/// writes into the buffer).
pub trait CanMapWrite: Sealed {}
impl CanMapWrite for MapReadWrite {}

/// A namespace mode that permits `lookup`/`enumerate`.
pub trait CanLookup: Sealed {}
impl CanLookup for NsReadOnly {}
impl CanLookup for NsMutable {}

/// A namespace mode that permits `bind` (also runtime-gated by the `BIND_NAMESPACE`
/// syscap, enforced kernel-side).
pub trait CanBind: Sealed {}
impl CanBind for NsMutable {}

/// A memory mode's `MAP_*` rights, used when mapping.
pub trait MemMode: Sealed {
    /// The `MAP_*` rights bits this mode maps with.
    fn map_rights() -> u64;
}
impl MemMode for MapRead {
    fn map_rights() -> u64 {
        Rights::MAP_READ.bits()
    }
}
impl MemMode for MapReadWrite {
    fn map_rights() -> u64 {
        (Rights::MAP_READ | Rights::MAP_WRITE).bits()
    }
}
impl MemMode for MapExec {
    fn map_rights() -> u64 {
        (Rights::MAP_READ | Rights::MAP_EXEC).bits()
    }
}

// --- the handle -----------------------------------------------------------

/// A typed capability handle. An **owning** handle (from [`from_raw`](Handle::from_raw))
/// closes its capability on drop; a **borrowed** one (from [`borrow`](Handle::borrow))
/// does not — a non-owning view of a handle owned elsewhere (e.g. the bootstrap
/// `root_ns`, which the process holds for its whole life).
pub struct Handle<T, M> {
    raw: RawHandle,
    extra: Rights,
    owned: bool,
    _t: PhantomData<T>,
    _m: PhantomData<M>,
}

impl<T, M> Handle<T, M> {
    /// Wrap a raw handle as a typed **owning** `Handle` (closed on drop). `extra` is
    /// the generic + modifier rights band.
    ///
    /// # Safety
    /// The caller asserts that `raw` names a live object of type `T` carrying at
    /// least mode `M`'s principal rights, and transfers ownership of it. libos cannot
    /// check `M` statically; where cheap, callers should confirm the object type via
    /// `sys_handle_stat` first.
    pub unsafe fn from_raw(raw: RawHandle, extra: Rights) -> Self {
        Handle {
            raw,
            extra,
            owned: true,
            _t: PhantomData,
            _m: PhantomData,
        }
    }

    /// Wrap a raw handle as a typed **borrowed** `Handle` — a non-owning view that is
    /// **not** closed on drop. For handles owned elsewhere (the bootstrap `root_ns`,
    /// a handle whose lifetime the caller manages).
    ///
    /// # Safety
    /// As [`from_raw`](Handle::from_raw), except ownership is **not** transferred: the
    /// caller must keep `raw` live for at least as long as the returned borrow, and
    /// must not attenuate it (that would mutate the owner's capability in place).
    pub unsafe fn borrow(raw: RawHandle, extra: Rights) -> Self {
        Handle {
            raw,
            extra,
            owned: false,
            _t: PhantomData,
            _m: PhantomData,
        }
    }

    /// The raw handle (for the raw paths that still need it — spawn ABIs, IPC handle
    /// transfer). Does **not** relinquish ownership; the `Handle` still closes on drop.
    pub fn raw(&self) -> RawHandle {
        self.raw
    }

    /// Extract the raw handle, relinquishing ownership (the `Handle` will **not** close
    /// it). Use before transferring the handle to a child or over IPC.
    pub fn into_raw(self) -> RawHandle {
        let raw = self.raw;
        core::mem::forget(self); // suppress Drop's close
        raw
    }

    /// The generic + modifier rights band (principal rights are implied by `M`).
    pub fn extra_rights(&self) -> Rights {
        self.extra
    }

    /// Restrict rights by intersecting with `mask` (via `sys_handle_restrict`) and
    /// retype the mode to `M2`. Consumes `self`; on error `self` is dropped (closed).
    fn retype<M2>(self, mask: u64) -> Result<Handle<T, M2>> {
        let r = sys::handle_restrict(self.raw.0, mask);
        if r < 0 {
            return Err(Error::from_status(r as i32));
        }
        let raw = self.raw;
        let extra = Rights::from_bits(self.extra.bits() & mask);
        let owned = self.owned;
        core::mem::forget(self); // keep the (now-restricted) capability alive under M2
        Ok(Handle {
            raw,
            extra,
            owned,
            _t: PhantomData,
            _m: PhantomData,
        })
    }

    /// Drop the `TRANSFER` right (this handle can no longer be sent over IPC).
    pub fn without_transfer(self) -> Result<Self> {
        let mask = !Rights::TRANSFER.bits();
        self.retype::<M>(mask)
    }

    /// Drop the `DUPLICATE` right.
    pub fn without_duplicate(self) -> Result<Self> {
        let mask = !Rights::DUPLICATE.bits();
        self.retype::<M>(mask)
    }
}

impl Handle<Memory, MapReadWrite> {
    /// Attenuate a read-write memory handle to read-only (drops `MAP_WRITE`).
    pub fn into_read_only(self) -> Result<Handle<Memory, MapRead>> {
        let mask = !Rights::MAP_WRITE.bits();
        self.retype::<MapRead>(mask)
    }
}

impl<T, M> Drop for Handle<T, M> {
    fn drop(&mut self) {
        if self.owned {
            sys::handle_close(self.raw.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_closes_the_capability() {
        sys::reset();
        {
            // SAFETY: test handle; the mock ignores the value.
            let _h = unsafe { Handle::<Memory, MapReadWrite>::from_raw(RawHandle(5), Rights::empty()) };
        } // dropped here
        let (_s, _w, closes) = sys::counts();
        assert_eq!(closes, 1, "Handle::drop must close the raw handle");
    }

    #[test]
    fn borrowed_handle_does_not_close_on_drop() {
        sys::reset();
        {
            // SAFETY: test handle; borrow does not take ownership.
            let _h = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(3), Rights::LOOKUP) };
        } // dropped here — must NOT close
        let (_s, _w, closes) = sys::counts();
        assert_eq!(closes, 0, "a borrowed Handle must not close on drop");
    }

    #[test]
    fn into_raw_suppresses_the_close() {
        sys::reset();
        // SAFETY: test handle.
        let h = unsafe { Handle::<Memory, MapReadWrite>::from_raw(RawHandle(5), Rights::empty()) };
        let raw = h.into_raw();
        assert_eq!(raw, RawHandle(5));
        let (_s, _w, closes) = sys::counts();
        assert_eq!(closes, 0, "into_raw must not close the handle");
    }

    #[test]
    fn attenuation_issues_a_restrict_and_keeps_the_handle() {
        sys::reset();
        // SAFETY: test handle.
        let h = unsafe {
            Handle::<Memory, MapReadWrite>::from_raw(
                RawHandle(5),
                Rights::TRANSFER | Rights::DUPLICATE,
            )
        };
        let h = h.without_transfer().unwrap();
        assert!(!h.extra_rights().contains(Rights::TRANSFER));
        assert!(h.extra_rights().contains(Rights::DUPLICATE));
        let (_s, _w, closes) = sys::counts();
        assert_eq!(closes, 0, "a successful restrict must not close the handle");
        // and the retyped handle still closes on drop
        drop(h);
        let (_s, _w, closes) = sys::counts();
        assert_eq!(closes, 1);
    }

    #[test]
    fn into_read_only_drops_map_write() {
        sys::reset();
        // SAFETY: test handle.
        let h = unsafe {
            Handle::<Memory, MapReadWrite>::from_raw(
                RawHandle(9),
                Rights::MAP_READ | Rights::MAP_WRITE,
            )
        };
        let _ro: Handle<Memory, MapRead> = h.into_read_only().unwrap();
        // Type is now Handle<Memory, MapRead> — a `write` op would not compile.
    }
}
