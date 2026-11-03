//! `Handle<T, M>`, a capability this process owns, and `Ref<'a, T, M>`, one it borrows.
//!
//! `T` is the object-type marker, `M` the mode marker encoding **principal** rights as a type (so
//! the wrong operation is a compile error), and `extra` carries the generic and modifier rights,
//! which the kernel checks. A `Handle` closes its capability on drop; a `Ref` is `Copy`, closes
//! nothing and attenuates nothing, and lives no longer than what it borrows. Operations are written
//! once for both ([`both!`]). Building either from a raw handle is `unsafe` for its ownership claim,
//! as `std`'s `OwnedFd` and `BorrowedFd` are: a wrong type or mode yields only the kernel's `Err`.

use core::marker::PhantomData;

use libkern::{HandleInfo, RawHandle, Rights};

use crate::error::{Result, check};
use crate::sys;

// --- object-type markers (T) ----------------------------------------------

/// A `MemoryObject` — anonymous mappable memory.
pub struct Memory;
/// A `FileObject` — a file's pages in the cache, mapped as memory is.
pub struct File;
/// A `Namespace` — a capability-scoped path→resource map.
pub struct Namespace;
/// A `NotificationChannel` — the async-event queue.
pub struct Notify;
/// An `IpcChannel` endpoint.
pub struct Channel;
/// A readable/writable resource (device or file) driven by `io_submit`.
pub struct Resource;
/// A child `Process` (from [`Spawn`](crate::Spawn)); closing the handle lets it go.
pub struct Process;
/// A `Thread` in this process (from [`thread::start`](crate::thread::start)).
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
seal!(Memory, File);

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

/// A namespace mode that permits `bind` (also runtime-gated by the `BIND_NAMESPACE` syscap,
/// enforced kernel-side).
pub trait CanBind: Sealed {}
impl CanBind for NsMutable {}

/// An object a process maps: memory, or a file's pages.
pub trait Mappable: Sealed {}
impl Mappable for Memory {}
impl Mappable for File {}

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

// --- the handle and the borrow --------------------------------------------

/// A typed capability this process owns, closed on drop.
pub struct Handle<T, M> {
    raw: RawHandle,
    extra: Rights,
    _t: PhantomData<(T, M)>,
}

/// A typed capability this process borrows: closed by its owner, never by this.
pub struct Ref<'a, T, M> {
    raw: RawHandle,
    extra: Rights,
    _t: PhantomData<(&'a (), T, M)>,
}

impl<T, M> Clone for Ref<'_, T, M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, M> Copy for Ref<'_, T, M> {}

impl<T, M> Handle<T, M> {
    /// Wrap a raw handle as an owning `Handle`, closed on drop. `extra` is the generic and
    /// modifier rights band.
    ///
    /// # Safety
    /// `raw` is this process's and nothing else closes or wraps it: ownership moves here.
    pub unsafe fn from_raw(raw: RawHandle, extra: Rights) -> Self {
        Handle { raw, extra, _t: PhantomData }
    }

    /// Wrap the handle a call has just made, which nothing else holds.
    pub(crate) fn fresh(raw: u64, extra: Rights) -> Self {
        Handle { raw: RawHandle(raw), extra, _t: PhantomData }
    }

    /// A borrow of this handle, for as long as it lives.
    pub fn as_ref(&self) -> Ref<'_, T, M> {
        Ref { raw: self.raw, extra: self.extra, _t: PhantomData }
    }

    /// Extract the raw handle, relinquishing ownership: the `Handle` will **not** close it.
    pub fn into_raw(self) -> RawHandle {
        let raw = self.raw;
        core::mem::forget(self);
        raw
    }

    /// Intersect rights with `mask` (`sys_handle_restrict`) and retype the mode to `M2`.
    /// On error `self` is dropped (closed).
    fn retype<M2>(self, mask: u64) -> Result<Handle<T, M2>> {
        check(sys::handle_restrict(self.raw.0, mask))?;
        let extra = Rights::from_bits(self.extra.bits() & mask);
        Ok(Handle::fresh(self.into_raw().0, extra))
    }

    /// Drop the `TRANSFER` right (this handle can no longer be sent over IPC).
    pub fn without_transfer(self) -> Result<Self> {
        self.retype::<M>(!Rights::TRANSFER.bits())
    }

    /// Drop the `DUPLICATE` right.
    pub fn without_duplicate(self) -> Result<Self> {
        self.retype::<M>(!Rights::DUPLICATE.bits())
    }
}

impl<T: Mappable> Handle<T, MapReadWrite> {
    /// Attenuate a read-write handle to read-only (drops `MAP_WRITE`).
    pub fn into_read_only(self) -> Result<Handle<T, MapRead>> {
        self.retype::<MapRead>(!Rights::MAP_WRITE.bits())
    }
}

impl<T, M> Drop for Handle<T, M> {
    fn drop(&mut self) {
        sys::handle_close(self.raw.0);
    }
}

impl<'a, T, M> Ref<'a, T, M> {
    /// Borrow a raw handle someone else owns.
    ///
    /// # Safety
    /// `raw` stays open for `'a`.
    pub unsafe fn from_raw(raw: RawHandle, extra: Rights) -> Self {
        Ref { raw, extra, _t: PhantomData }
    }
}

impl<'a, T, M> From<&'a Handle<T, M>> for Ref<'a, T, M> {
    fn from(h: &'a Handle<T, M>) -> Self {
        h.as_ref()
    }
}

/// Write each operation once, for both an owned [`Handle`] and a borrowed [`Ref`].
macro_rules! both {
    ([$($g:tt)*] $t:ty, $m:ty { $($body:tt)* }) => {
        impl<$($g)*> $crate::handle::Handle<$t, $m> { $($body)* }
        impl<'r, $($g)*> $crate::handle::Ref<'r, $t, $m> { $($body)* }
    };
}
pub(crate) use both;

both!([T, M] T, M {
    /// The raw handle, still owned by whoever owned it.
    pub fn raw(&self) -> RawHandle {
        self.raw
    }

    /// The generic + modifier rights band (principal rights are implied by `M`).
    pub fn extra_rights(&self) -> Rights {
        self.extra
    }

    /// The handle's rights, object type and size (`sys_handle_stat`); needs `INSPECT`.
    pub fn stat(&self) -> Result<HandleInfo> {
        let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
        check(sys::handle_stat(self.raw.0, &mut info))?;
        Ok(info)
    }

    /// A second handle to the same object with at most `rights`; needs `DUPLICATE`.
    pub fn duplicate(&self, rights: Rights) -> Result<Handle<T, M>> {
        let h = check(sys::handle_duplicate(self.raw.0, rights.bits()))?;
        Ok(Handle::fresh(h, Rights::from_bits(self.extra.bits() & rights.bits())))
    }
});

// --- handles in flight ----------------------------------------------------

/// Up to `N` handles this process owns on their way somewhere, or just arrived: those a send or
/// a spawn moves, and those a receive brought. Each still here when it drops is closed.
pub struct Handles<const N: usize> {
    raw: [u64; N],
    extra: [Rights; N],
    len: usize,
}

impl<const N: usize> Default for Handles<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Handles<N> {
    /// None.
    pub const fn new() -> Self {
        Handles { raw: [0; N], extra: [Rights::empty(); N], len: 0 }
    }

    /// Add `h` at the end; handed back if all `N` places are taken.
    pub fn push<T, M>(&mut self, h: Handle<T, M>) -> core::result::Result<(), Handle<T, M>> {
        if self.len == N {
            return Err(h);
        }
        self.extra[self.len] = h.extra;
        self.raw[self.len] = h.into_raw().0;
        self.len += 1;
        Ok(())
    }

    /// How many places are filled, taken or not.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no place is filled.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Take handle `i` as a `T` of mode `M`: it is the caller's now. `None` past the count, or
    /// if taken already.
    pub fn take<T, M>(&mut self, i: usize) -> Option<Handle<T, M>> {
        let raw = core::mem::take(self.raw[..self.len].get_mut(i)?);
        (raw != 0).then(|| Handle::fresh(raw, self.extra[i]))
    }

    /// The raw values, `0` where taken, for a call that moves them.
    pub(crate) fn raw(&self) -> &[u64] {
        &self.raw[..self.len]
    }

    /// The kernel moved them all: none is this process's to close.
    pub(crate) fn moved(&mut self) {
        self.raw = [0; N];
        self.len = 0;
    }

    /// What a receive brought: `count` handles, now this process's.
    pub(crate) fn arrived(raw: [u64; N], count: usize) -> Self {
        Handles { raw, extra: [Rights::empty(); N], len: count.min(N) }
    }
}

impl<const N: usize> Drop for Handles<N> {
    fn drop(&mut self) {
        for &h in self.raw() {
            if h != 0 {
                sys::handle_close(h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Handle<Memory, MapReadWrite> {
        Handle::fresh(sys::memory_create(4096) as u64, Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT)
    }

    #[test]
    fn drop_closes_the_capability_and_a_borrow_does_not() {
        sys::reset();
        let h = memory();
        let raw = h.raw().0;
        let r = h.as_ref();
        let copy = r;
        assert_eq!(copy.raw(), h.raw());
        drop(h);
        assert!(!sys::is_open(raw), "Handle::drop closes the raw handle");
        sys::reset();
        let h = memory();
        // SAFETY: `h` outlives the borrow.
        let r = unsafe { Ref::<Memory, MapReadWrite>::from_raw(h.raw(), Rights::empty()) };
        let _ = r;
        assert!(sys::is_open(h.raw().0), "a borrow closes nothing");
    }

    #[test]
    fn into_raw_suppresses_the_close() {
        sys::reset();
        let h = memory();
        let raw = h.into_raw();
        assert!(sys::is_open(raw.0));
    }

    #[test]
    fn attenuation_restricts_in_place_and_a_refusal_closes() {
        sys::reset();
        let h = memory().without_transfer().unwrap();
        assert!(!h.extra_rights().contains(Rights::TRANSFER));
        assert!(h.extra_rights().contains(Rights::DUPLICATE));
        assert_eq!(h.stat().unwrap().rights & Rights::TRANSFER.bits(), 0, "the kernel's rights, not just ours");
        let ro = h.into_read_only().unwrap();
        assert_eq!(ro.stat().unwrap().rights & Rights::MAP_WRITE.bits(), 0);
    }

    #[test]
    fn a_duplicate_is_a_second_handle_and_needs_duplicate() {
        sys::reset();
        let h = memory();
        let d = h.duplicate(Rights::MAP_READ | Rights::INSPECT).unwrap();
        assert_ne!(d.raw(), h.raw());
        assert_eq!(d.stat().unwrap().rights, (Rights::MAP_READ | Rights::INSPECT).bits());
        let e = d.duplicate(Rights::MAP_READ).err().unwrap();
        assert_eq!(e.code(), libkern::KError::NoAccess, "no DUPLICATE on the copy");
    }

    #[test]
    fn stat_needs_inspect_and_a_closed_handle_is_invalid() {
        sys::reset();
        let h = memory();
        assert_eq!(h.stat().unwrap().size, 4096);
        let raw = h.raw();
        drop(h);
        // SAFETY: the mock answers for a closed handle; nothing uses it as memory.
        let r = unsafe { Ref::<Memory, MapRead>::from_raw(raw, Rights::empty()) };
        assert_eq!(r.stat().err().unwrap().code(), libkern::KError::InvalidHandle);
    }

    #[test]
    fn handles_close_what_was_not_taken_once() {
        sys::reset();
        let mut hs = Handles::<2>::new();
        let (a, b) = (memory(), memory());
        let (ra, rb) = (a.raw().0, b.raw().0);
        hs.push(a).ok().unwrap();
        hs.push(b).ok().unwrap();
        assert!(hs.push(memory()).is_err(), "no third place");
        let taken: Handle<Memory, MapRead> = hs.take(1).unwrap();
        assert!(hs.take::<Memory, MapRead>(1).is_none() && hs.take::<Memory, MapRead>(2).is_none());
        drop(hs);
        assert!(!sys::is_open(ra));
        assert!(sys::is_open(rb) && taken.raw().0 == rb);
    }
}
