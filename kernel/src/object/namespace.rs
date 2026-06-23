//! The [`Namespace`] kernel object — a per-process map from paths to resources.
//!
//! A namespace is an ordered set of **bindings** `{ path, target, rights }`;
//! resolving a path is a **longest-prefix match** that yields the covering
//! binding and the remaining *suffix*. It is the per-process name-resolution
//! substrate that replaces a global VFS — see
//! `docs/architecture/namespace-and-resource-servers.md` for the full model.
//!
//! This module is the slice-1 substrate: the object, the binding store, path
//! validation, and the resolver. The `sys_ns_*` syscalls (Part C) and
//! resource-server IPC-forwarding (slice 3) build on it. Slice-1 binding targets
//! are **direct handles** (a bound `ObjectRef`); the `KernelServer`/`UserspaceServer`/`SubNamespace`
//! target kinds — and a `BindingTarget` enum — arrive with slice 3.
//!
//! ## Mutation discipline
//!
//! Unlike `Timer`/`IpcChannel` (touched by the scheduler, hence `UnsafeCell`
//! under `SCHED`), a `Namespace` is touched only by the owning thread's syscalls.
//! Its state lives behind a plain [`SpinLock`] at lock-rank 4 — the `AddressSpace`
//! model (`kernel/docs/lock-ordering.md`). `bind` may allocate under that lock
//! (rank 4 → the rank-6 allocator is a legal acquire order; the no-allocation rule
//! is rank-1 `SCHED` only). **No `ObjectRef` is ever dropped while the lock is
//! held** (a target's `Drop` could take a higher-rank lock — e.g. an `IpcChannel`
//! endpoint closing takes `SCHED`): bind/unbind hand refs back to the caller to
//! drop outside the lock; resolve clones (an atomic bump, never a drop).

use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::{AllocError, KBox, KVec, SpinLock};
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

/// Maximum namespace path length in bytes (see the design doc § Path grammar).
pub const NS_PATH_MAX: usize = 1024;

/// Capacity of each namespace's resolution cache (design doc § The lookup cache).
/// Small and pre-reserved; resolution is a hot path, mutations are rare.
const NS_CACHE_MAX: usize = 8;

/// Why a namespace operation failed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NsError {
    /// The path is not a valid namespace path (see [`validate_path`]).
    InvalidPath,
    /// A binding already exists at this exact path.
    AlreadyBound,
    /// No binding exists at this exact path (unbind).
    NotBound,
    /// A backing allocation failed.
    OutOfMemory,
}

/// One binding: the absolute path it owns, the resource it resolves to, and the
/// maximum rights a lookup through it may obtain.
struct Binding {
    path: KVec<u8>,
    target: ObjectRef,
    rights: Rights,
}

/// A cached positive resolution: a fully looked-up `path` and the index of the
/// `bindings` entry it resolved to. Holds **no** `ObjectRef` (so a cache flush is
/// a byte-only `KVec` drop — never an `ObjectRef` drop under the lock). The index
/// is valid because every binding mutation flushes the whole cache.
struct CacheEntry {
    path: KVec<u8>,
    binding_index: usize,
}

/// A per-process namespace kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct Namespace {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`Namespace::MAGIC`].
    magic: u64,
    inner: SpinLock<Inner>,
}

struct Inner {
    bindings: KVec<Binding>,
    /// Bounded resolution cache (pre-reserved to [`NS_CACHE_MAX`]); flushed whole
    /// on any binding mutation. A pure optimization — no semantic effect.
    cache: KVec<CacheEntry>,
}

impl Namespace {
    /// Sentinel written into [`Namespace::magic`] at construction.
    pub const MAGIC: u64 = 0x4e_61_6d_65_73_70_63_21; // "Namespc!"

    /// Allocate an empty namespace with a refcount of one. The resolution cache
    /// is pre-reserved to [`NS_CACHE_MAX`] so cache inserts never need to grow it.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut cache: KVec<CacheEntry> = KVec::new();
        cache.try_reserve(NS_CACHE_MAX)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Namespace),
            magic: Self::MAGIC,
            inner: SpinLock::new(Inner { bindings: KVec::new(), cache }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Bind `target` (a direct handle) at `path` with `rights` (the maximum a
    /// lookup through it may obtain). Rejects an invalid path or a duplicate exact
    /// path; on any error the `target` is **handed back** in the tuple so the
    /// caller drops it outside the lock (never dropped here — see "Mutation
    /// discipline").
    pub fn bind(
        &self,
        path: &[u8],
        target: ObjectRef,
        rights: Rights,
    ) -> Result<(), (ObjectRef, NsError)> {
        if let Err(e) = validate_path(path) {
            return Err((target, e));
        }
        let mut guard = self.inner.lock();
        if guard.bindings.iter().any(|b| &b.path[..] == path) {
            return Err((target, NsError::AlreadyBound));
        }
        // Reserve the slot and copy the path BEFORE moving `target` into a
        // `Binding`, so the committing `try_push` cannot fail (and therefore
        // cannot drop the `Binding` — and its `ObjectRef` — under the lock).
        if guard.bindings.try_reserve(1).is_err() {
            return Err((target, NsError::OutOfMemory));
        }
        let mut p: KVec<u8> = KVec::new();
        if p.try_extend_from_slice(path).is_err() {
            // `p` holds only bytes (no `ObjectRef`); dropping it here is fine.
            return Err((target, NsError::OutOfMemory));
        }
        guard
            .bindings
            .try_push(Binding { path: p, target, rights })
            .expect("slot reserved above");
        // A mutation may change what any cached path resolves to; flush the whole
        // cache (cheap: byte-only `KVec`s, no `ObjectRef` drops). Mutations are rare.
        guard.cache.clear();
        Ok(())
    }

    /// Remove the binding at the exact `path`, returning its `target` for the
    /// caller to drop **outside** the lock. `None` if nothing is bound there.
    pub fn unbind(&self, path: &[u8]) -> Option<ObjectRef> {
        let mut guard = self.inner.lock();
        let idx = guard.bindings.iter().position(|b| &b.path[..] == path)?;
        // `remove` moves the `Binding` out by value (no drop). We move its
        // `target` into the return; the rest (`path` bytes, `rights`) drops here —
        // no `ObjectRef` among them, so the lock-held drop is harmless.
        let b = guard.bindings.remove(idx);
        // Removing a binding shifts indices and changes resolutions; flush the
        // whole cache (byte-only `KVec` drops, no `ObjectRef` drops).
        guard.cache.clear();
        Some(b.target)
    }

    /// Resolve `path` by **longest-prefix match**. Returns a **cloned** target
    /// `ObjectRef` (an atomic refcount bump under the lock — keeps the resource
    /// alive for the caller), the binding's `rights`, and the `suffix` of `path`
    /// past the matched prefix (leading `/` stripped; empty on an exact match).
    /// `None` if no binding covers `path`.
    ///
    /// Pure resolution: the direct-handle leaf policy (a non-empty suffix on a
    /// direct handle is *not found*) and rights attenuation are the lookup
    /// syscall's job (Part C).
    ///
    /// Positive resolutions are cached (path → binding index); a repeat lookup of
    /// the same path skips the longest-prefix scan. The cache is flushed on every
    /// binding mutation, so a cached index always refers to the same binding.
    pub fn resolve<'p>(&self, path: &'p [u8]) -> Option<(ObjectRef, Rights, &'p [u8])> {
        let mut guard = self.inner.lock();

        // Cache fast path: an exact prior lookup of this path. The cached index is
        // valid (mutations flush the cache), so recompute the suffix and return.
        if let Some(idx) = guard
            .cache
            .iter()
            .find(|e| &e.path[..] == path)
            .map(|e| e.binding_index)
        {
            let b = &guard.bindings[idx];
            let off = match_suffix_offset(&b.path, path)
                .expect("a cached binding still prefix-matches its cached path");
            return Some((b.target.clone(), b.rights, &path[off..]));
        }

        // Cold path: longest-prefix scan over the bindings.
        let mut best: Option<(usize, usize)> = None; // (binding index, suffix offset)
        for (i, b) in guard.bindings.iter().enumerate() {
            let Some(off) = match_suffix_offset(&b.path, path) else {
                continue;
            };
            match best {
                // Keep the longer binding path (the more specific match).
                Some((bi, _)) if guard.bindings[bi].path.len() >= b.path.len() => {}
                _ => best = Some((i, off)),
            }
        }
        let (i, off) = best?;
        let result = {
            let b = &guard.bindings[i];
            (b.target.clone(), b.rights, &path[off..])
        };
        // Insert into the cache (best-effort): copy the path, round-robin evict
        // when full. A failed path copy just skips caching — no correctness effect.
        // No `ObjectRef` is stored, so neither insert nor the evicting `remove(0)`
        // drops one under the lock.
        let mut entry_path: KVec<u8> = KVec::new();
        if entry_path.try_extend_from_slice(path).is_ok() {
            if guard.cache.len() == NS_CACHE_MAX {
                guard.cache.remove(0);
            }
            guard
                .cache
                .try_push(CacheEntry { path: entry_path, binding_index: i })
                .expect("cache pre-reserved to NS_CACHE_MAX; one slot freed above");
        }
        Some(result)
    }

    /// The number of live resolution-cache entries. Test-only observability.
    #[cfg(test)]
    pub(crate) fn cache_len(&self) -> usize {
        self.inner.lock().cache.len()
    }
}

// No `Drop` impl: the `KBox` drop (run by `dispatch_destroy`, outside any lock)
// drops `inner` → the `bindings` `KVec` → each `Binding`'s `target` `ObjectRef`,
// releasing every bound resource. (`Namespace` auto-derives `Send`/`Sync`: all
// fields are — `ObjectRef` is `Send`/`Sync`, `SpinLock<T>: Sync` for `T: Send`.)

/// Validate a namespace path: non-empty, absolute (`/`-prefixed), ≤
/// [`NS_PATH_MAX`], and — except for root `/` — every `/`-separated component
/// non-empty and not `.`/`..`, with no trailing `/`. (Callers normalize; the
/// kernel never interprets `.`/`..`, so path traversal cannot occur.)
pub fn validate_path(path: &[u8]) -> Result<(), NsError> {
    if path.is_empty() || path.len() > NS_PATH_MAX || path[0] != b'/' {
        return Err(NsError::InvalidPath);
    }
    if path == b"/" {
        return Ok(()); // root
    }
    if path[path.len() - 1] == b'/' {
        return Err(NsError::InvalidPath); // trailing slash
    }
    for comp in path[1..].split(|&c| c == b'/') {
        if comp.is_empty() || comp == b"." || comp == b".." {
            return Err(NsError::InvalidPath);
        }
    }
    Ok(())
}

/// If `binding` is a component-boundary prefix of the absolute `query`, return
/// the byte offset in `query` where the suffix begins (past any boundary `/`);
/// `None` otherwise. Root (`/`) matches every absolute query.
fn match_suffix_offset(binding: &[u8], query: &[u8]) -> Option<usize> {
    if binding == b"/" {
        // Suffix is everything after the leading '/'.
        return Some(1.min(query.len()));
    }
    if query.len() < binding.len() || &query[..binding.len()] != binding {
        return None;
    }
    if query.len() == binding.len() {
        return Some(query.len()); // exact match → empty suffix
    }
    // The prefix must end on a component boundary.
    if query[binding.len()] == b'/' {
        Some(binding.len() + 1) // skip the boundary '/'
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::Timer;
    use crate::object::header::test_probe;

    /// A live `Timer`, adopted into an `ObjectRef`, to use as a bind target.
    fn target() -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::Timer,
            )
        }
    }

    fn ns() -> KBox<Namespace> {
        init_global_heap();
        Namespace::try_new().unwrap()
    }

    #[test]
    fn validate_path_accepts_and_rejects() {
        assert!(validate_path(b"/").is_ok());
        assert!(validate_path(b"/dev").is_ok());
        assert!(validate_path(b"/dev/log").is_ok());
        for bad in [
            &b""[..],
            b"dev",      // relative
            b"/dev/",    // trailing slash
            b"//dev",    // empty component
            b"/./x",     // dot
            b"/../x",    // dotdot
            b"/a//b",    // empty interior component
        ] {
            assert_eq!(validate_path(bad), Err(NsError::InvalidPath), "{bad:?}");
        }
        let toolong = [b'/'; NS_PATH_MAX + 1];
        assert_eq!(validate_path(&toolong), Err(NsError::InvalidPath));
    }

    #[test]
    fn bind_rejects_invalid_and_duplicate() {
        let n = ns();
        assert!(n.bind(b"/dev", target(), Rights::LOOKUP).is_ok());
        // Invalid path: target handed back.
        let (t, e) = n.bind(b"bad", target(), Rights::LOOKUP).unwrap_err();
        assert_eq!(e, NsError::InvalidPath);
        drop(t);
        // Duplicate exact path: target handed back.
        let (t, e) = n.bind(b"/dev", target(), Rights::LOOKUP).unwrap_err();
        assert_eq!(e, NsError::AlreadyBound);
        drop(t);
    }

    #[test]
    fn unbind_returns_target_and_allows_rebind() {
        let n = ns();
        n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
        let t = n.unbind(b"/dev").expect("was bound");
        drop(t); // released outside the lock
        assert!(n.unbind(b"/dev").is_none(), "already removed");
        // Re-bind now succeeds.
        assert!(n.bind(b"/dev", target(), Rights::LOOKUP).is_ok());
    }

    #[test]
    fn resolve_exact_prefix_and_longest_match() {
        let n = ns();
        n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
        n.bind(b"/dev/log", target(), Rights::LOOKUP | Rights::INSPECT).unwrap();
        n.bind(b"/store", target(), Rights::LOOKUP).unwrap();

        // Exact match → empty suffix.
        let (_, _, suf) = n.resolve(b"/store").unwrap();
        assert_eq!(suf, b"");
        // Prefix match → suffix past the boundary.
        let (_, _, suf) = n.resolve(b"/dev/tty0").unwrap();
        assert_eq!(suf, b"tty0");
        // Longest of several: /dev/log wins for /dev/log (and carries its rights).
        let (_, rights, suf) = n.resolve(b"/dev/log").unwrap();
        assert_eq!(suf, b"");
        assert_eq!(rights, Rights::LOOKUP | Rights::INSPECT);
    }

    #[test]
    fn cache_populates_hits_and_flushes_on_mutation() {
        let n = ns();
        n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
        n.bind(b"/dev/log", target(), Rights::LOOKUP | Rights::INSPECT).unwrap();
        assert_eq!(n.cache_len(), 0, "bind leaves the cache empty");

        // A cold resolve caches the path; a repeat is a cache hit (same result).
        let (_, r0, s0) = n.resolve(b"/dev/log").unwrap();
        assert_eq!(n.cache_len(), 1);
        let (_, r1, s1) = n.resolve(b"/dev/log").unwrap();
        assert_eq!(n.cache_len(), 1, "repeat lookup reuses the entry");
        assert_eq!((r0, s0), (r1, s1), "cache hit matches the cold resolve");

        // A prefix lookup caches a distinct entry.
        let (_, _, suf) = n.resolve(b"/dev/tty0").unwrap();
        assert_eq!(suf, b"tty0");
        assert_eq!(n.cache_len(), 2);

        // Any mutation flushes the whole cache.
        n.bind(b"/store", target(), Rights::LOOKUP).unwrap();
        assert_eq!(n.cache_len(), 0, "bind flushes the cache");
        n.resolve(b"/store").unwrap();
        assert_eq!(n.cache_len(), 1);
        let t = n.unbind(b"/store").expect("bound");
        drop(t);
        assert_eq!(n.cache_len(), 0, "unbind flushes the cache");
    }

    #[test]
    fn cache_evicts_at_capacity() {
        let n = ns();
        n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
        // Resolve more distinct paths than the cache holds; it caps at NS_CACHE_MAX.
        for i in 0..(NS_CACHE_MAX as u8 + 4) {
            let path = [b'/', b'd', b'e', b'v', b'/', b'a' + i];
            let (_, _, suf) = n.resolve(&path).unwrap();
            assert_eq!(suf, &path[5..]); // single trailing component
        }
        assert_eq!(n.cache_len(), NS_CACHE_MAX, "cache is bounded");
    }

    #[test]
    fn resolve_respects_component_boundary_and_misses() {
        let n = ns();
        n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
        // `/dev` must NOT match `/devices` (not a component boundary).
        assert!(n.resolve(b"/devices").is_none());
        // Nothing covers `/net`.
        assert!(n.resolve(b"/net/x").is_none());
    }

    #[test]
    fn resolve_root_matches_everything() {
        let n = ns();
        n.bind(b"/", target(), Rights::LOOKUP).unwrap();
        let (_, _, suf) = n.resolve(b"/").unwrap();
        assert_eq!(suf, b"");
        let (_, _, suf) = n.resolve(b"/anything/here").unwrap();
        assert_eq!(suf, b"anything/here");
    }

    #[test]
    fn resolved_target_outlives_the_namespace() {
        init_global_heap();
        test_probe::reset();
        let resolved;
        {
            let n = Namespace::try_new().unwrap();
            n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
            let (t, _, _) = n.resolve(b"/dev").unwrap();
            resolved = t; // a cloned ref — keeps the Timer alive
            // Drop the namespace (KBox): its binding's target ref is released,
            // but `resolved` still holds one, so the Timer is NOT destroyed yet.
            drop(n);
        }
        assert_eq!(test_probe::timer_destroys(), 0, "still pinned by the clone");
        drop(resolved);
        assert_eq!(test_probe::timer_destroys(), 1);
    }

    #[test]
    fn dropping_namespace_frees_all_targets() {
        init_global_heap();
        test_probe::reset();
        // 8 build/drop cycles, 3 bindings each — a leak would show in the count.
        for _ in 0..8 {
            let n = Namespace::try_new().unwrap();
            n.bind(b"/a", target(), Rights::LOOKUP).unwrap();
            n.bind(b"/b", target(), Rights::LOOKUP).unwrap();
            n.bind(b"/c", target(), Rights::LOOKUP).unwrap();
            drop(n); // the namespace's 3 target refs are released
        }
        assert_eq!(test_probe::timer_destroys(), 24);
    }

    #[test]
    fn dropping_last_objectref_routes_through_dispatch_destroy() {
        init_global_heap();
        test_probe::reset();
        // Adopt the namespace into an `ObjectRef` — the path a real handle
        // release takes — and drop it. The last reference runs
        // `dispatch_destroy`'s `Namespace` arm, which frees the `KBox` and
        // cascades to the two bound targets.
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let r = unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(Namespace::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::Namespace,
            )
        };
        // Reach the object through the ref to populate it.
        {
            // SAFETY: `r` holds a live reference to a `Namespace`.
            let ns: &Namespace = unsafe { &*(r.as_ptr() as *const Namespace) };
            ns.bind(b"/a", target(), Rights::LOOKUP).unwrap();
            ns.bind(b"/b", target(), Rights::LOOKUP).unwrap();
        }
        assert_eq!(test_probe::namespace_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::namespace_destroys(), 1, "namespace destructor ran");
        assert_eq!(test_probe::timer_destroys(), 2, "bound targets cascaded");
    }
}
