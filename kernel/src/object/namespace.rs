//! The [`Namespace`] kernel object — a per-process map from paths to resources.
//!
//! A namespace is an ordered set of **bindings** `{ path, target, rights }`; resolving a path
//! is a **longest-prefix match** yielding the covering binding and the remaining *suffix*. See
//! `docs/architecture/namespace-and-resource-servers.md`.
//!
//! ## Mutation discipline
//!
//! A `Namespace` is touched only by the owning thread's syscalls (unlike `Timer`/`IpcChannel`),
//! so its state lives behind a plain [`SpinLock`] at lock-rank 4 — the `AddressSpace` model
//! (`kernel/docs/lock-ordering.md`). `bind` may allocate under it (rank 4 → the rank-6
//! allocator is legal). **No `ObjectRef` is ever dropped while the lock is held** (a target's
//! `Drop` could take a higher-rank lock: an `IpcChannel` endpoint closing takes `SCHED`):
//! bind/unbind hand refs back to the caller to drop outside the lock; resolve clones (an atomic
//! bump, never a drop).

use crate::libkern::handle::{
    KObjectType, NS_ENTRY_PATH_MAX, NS_KIND_DIRECT, NS_KIND_KERNEL, NS_KIND_MOUNT, NsEntry, Rights,
};
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, KVec, SpinLock};
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;
use crate::object::kernel_server::KernelServerId;

/// Maximum namespace path length in bytes (see the design doc § Path grammar).
pub const NS_PATH_MAX: usize = 1024;

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

/// Maximum length of a subtree base path, in bytes. Bases are short; the bound keeps
/// [`SubtreeBase`] a small, `Copy`, allocation-free inline value.
pub const NS_SUBTREE_BASE_MAX: usize = 128;

/// A **subtree base path** attached to a userspace-server binding: the server-root-relative
/// prefix the kernel prepends to every forwarded lookup's suffix, so the binding exposes only a
/// *sub-tree* of the server (see `docs/architecture/namespace-and-resource-servers.md` §
/// Subtree scoping). Empty = no scoping. A small, `Copy`, allocation-free buffer owning no
/// `ObjectRef`, so [`Namespace::resolve`] can copy it out from under the lock; the stored path
/// is a validated absolute path, its leading `/` stripped where the kernel prepends it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct SubtreeBase {
    buf: [u8; NS_SUBTREE_BASE_MAX],
    len: u16,
}

impl SubtreeBase {
    /// No scoping — the whole server tree is exposed.
    pub const fn empty() -> Self {
        SubtreeBase { buf: [0u8; NS_SUBTREE_BASE_MAX], len: 0 }
    }

    /// Build a base from an absolute namespace `path`, validated like any binding path (so it can
    /// never escape a subtree); root `/` and a path over [`NS_SUBTREE_BASE_MAX`] are rejected.
    pub fn from_path(path: &[u8]) -> Result<Self, NsError> {
        validate_path(path)?;
        if path == b"/" || path.len() > NS_SUBTREE_BASE_MAX {
            return Err(NsError::InvalidPath);
        }
        let mut b = SubtreeBase::empty();
        b.buf[..path.len()].copy_from_slice(path);
        b.len = path.len() as u16;
        Ok(b)
    }

    /// The stored absolute base path (leading `/`), empty when unscoped.
    pub fn as_path(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }

    /// True when no scoping is applied.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// What a binding resolves to: a **direct handle** (a bound `ObjectRef`), a **Kernel Server**
/// (an in-kernel server the lookup calls to *produce* a handle) or a **Userspace Server** (a
/// process reached over IPC; the lookup is forwarded) with a [`SubtreeBase`]. `DirectHandle`
/// and `UserspaceServer` own an `ObjectRef` and must be dropped **outside** the namespace lock;
/// `KernelServer` and `SubtreeBase` are `Copy` and drop-free.
pub enum BindingTarget {
    /// A directly-bound resource. A lookup returns a clone of this handle.
    DirectHandle(ObjectRef),
    /// An in-kernel resource server, identified by its dispatch id.
    KernelServer(KernelServerId),
    /// A userspace resource server via its kernel registration record, with a subtree base.
    UserspaceServer(ObjectRef, SubtreeBase),
}

/// The result of [`Namespace::resolve`]: the kinds of [`BindingTarget`], the handle-owning cases
/// carrying a **cloned** `ObjectRef` (an atomic bump) for the caller to drop **outside** the lock.
pub enum ResolvedTarget {
    /// A cloned direct-handle resource, owned by the caller.
    DirectHandle(ObjectRef),
    /// An in-kernel resource server to dispatch to (id copied out).
    KernelServer(KernelServerId),
    /// A cloned reference to a userspace server's registration, plus the binding's [`SubtreeBase`]
    /// the caller prepends to the forwarded suffix.
    UserspaceServer(ObjectRef, SubtreeBase),
}

struct Binding {
    path: KVec<u8>,
    target: BindingTarget,
    rights: Rights,
}

/// A cached positive resolution: a `path` and the `bindings` index it resolved to. Holds **no**
/// `ObjectRef`, so a flush never drops one under the lock; the index stays valid because every
/// binding mutation flushes the whole cache.
struct CacheEntry {
    path: KVec<u8>,
    binding_index: usize,
}

/// A per-process namespace kernel object.
#[repr(C)]
pub struct Namespace {
    header: KObjectHeader,
    magic: u64,
    inner: SpinLock<Inner>,
}

struct Inner {
    bindings: KVec<Binding>,
    cache: KVec<CacheEntry>,
}

impl Namespace {
    /// Sentinel written into [`Namespace::magic`] at construction.
    pub const MAGIC: u64 = 0x4e_61_6d_65_73_70_63_21; // "Namespc!"

    /// Allocate an empty namespace, refcount one, with the resolution cache pre-reserved.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut cache: KVec<CacheEntry> = KVec::new();
        cache.try_reserve(NS_CACHE_MAX)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Namespace),
            magic: Self::MAGIC,
            inner: SpinLock::new(LockRank::KernelObject, Inner { bindings: KVec::new(), cache }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Bind `target` (a direct handle) at `path` with `rights` (the maximum a lookup through it
    /// may obtain). On an invalid path or duplicate exact path, `target` is **handed back** so the
    /// caller drops it outside the lock.
    pub fn bind(&self, path: &[u8], target: ObjectRef, rights: Rights) -> Result<(), (ObjectRef, NsError)> {
        if let Err(e) = validate_path(path) {
            return Err((target, e));
        }
        let mut guard = self.inner.lock();
        if guard.bindings.iter().any(|b| &b.path[..] == path) {
            return Err((target, NsError::AlreadyBound));
        }
        // Reserve the slot and copy the path BEFORE moving `target` into a `Binding`, so the commit
        // cannot fail and so cannot drop the `Binding` (and its `ObjectRef`) under the lock.
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
            .try_push(Binding { path: p, target: BindingTarget::DirectHandle(target), rights })
            .expect("slot reserved above");
        guard.cache.clear();
        Ok(())
    }

    /// Bind an in-kernel resource server (a [`KernelServerId`]) at `path` with `rights`; like
    /// [`bind`](Namespace::bind) but the target is a `Copy` id, so nothing is handed back.
    pub fn bind_kernel_server(&self, path: &[u8], id: KernelServerId, rights: Rights) -> Result<(), NsError> {
        validate_path(path)?;
        let mut guard = self.inner.lock();
        if guard.bindings.iter().any(|b| &b.path[..] == path) {
            return Err(NsError::AlreadyBound);
        }
        // Reserve + copy the path before committing, so the `try_push` cannot fail.
        if guard.bindings.try_reserve(1).is_err() {
            return Err(NsError::OutOfMemory);
        }
        let mut p: KVec<u8> = KVec::new();
        if p.try_extend_from_slice(path).is_err() {
            return Err(NsError::OutOfMemory);
        }
        guard
            .bindings
            .try_push(Binding { path: p, target: BindingTarget::KernelServer(id), rights })
            .expect("slot reserved above");
        guard.cache.clear();
        Ok(())
    }

    /// Bind a userspace resource server (its registration `reg`) at `path` with `rights`; like
    /// [`bind`](Namespace::bind) but `reg` is **handed back** on error. `base` is the subtree
    /// scoping prefix ([`SubtreeBase::empty`] for a whole-tree mount) prepended to a lookup's
    /// suffix before it is forwarded.
    pub fn bind_userspace_server(
        &self,
        path: &[u8],
        reg: ObjectRef,
        rights: Rights,
        base: SubtreeBase,
    ) -> Result<(), (ObjectRef, NsError)> {
        if let Err(e) = validate_path(path) {
            return Err((reg, e));
        }
        let mut guard = self.inner.lock();
        if guard.bindings.iter().any(|b| &b.path[..] == path) {
            return Err((reg, NsError::AlreadyBound));
        }
        // Reserve + copy the path before committing, so `reg` is never dropped under the lock.
        if guard.bindings.try_reserve(1).is_err() {
            return Err((reg, NsError::OutOfMemory));
        }
        let mut p: KVec<u8> = KVec::new();
        if p.try_extend_from_slice(path).is_err() {
            return Err((reg, NsError::OutOfMemory));
        }
        guard
            .bindings
            .try_push(Binding { path: p, target: BindingTarget::UserspaceServer(reg, base), rights })
            .expect("slot reserved above");
        guard.cache.clear();
        Ok(())
    }

    /// Remove the binding at the exact `path`, returning its `target` for the caller to drop
    /// **outside** the lock (it may carry an `ObjectRef`). `None` if nothing is bound there.
    pub fn unbind(&self, path: &[u8]) -> Option<BindingTarget> {
        let mut guard = self.inner.lock();
        let idx = guard.bindings.iter().position(|b| &b.path[..] == path)?;
        // `remove` moves the `Binding` out; `target` is returned, the rest holds no `ObjectRef`.
        let b = guard.bindings.remove(idx);
        guard.cache.clear();
        Some(b.target)
    }

    /// Resolve `path` by **longest-prefix match**: the matched [`ResolvedTarget`] (a **cloned**
    /// `ObjectRef` — an atomic bump under the lock — or the copied [`KernelServerId`]), the
    /// binding's `rights`, and the `suffix` past the matched prefix (leading `/` stripped; empty on
    /// an exact match). `None` if no binding covers `path`.
    ///
    /// Pure resolution: the direct-handle leaf policy, kernel-server dispatch and rights
    /// attenuation are the lookup syscall's job. Positive resolutions are cached (path → binding
    /// index); every binding mutation flushes the cache, so a cached index stays correct.
    pub fn resolve<'p>(&self, path: &'p [u8]) -> Option<(ResolvedTarget, Rights, &'p [u8])> {
        let mut guard = self.inner.lock();

        if let Some(idx) = guard.cache.iter().find(|e| &e.path[..] == path).map(|e| e.binding_index) {
            let b = &guard.bindings[idx];
            let off =
                match_suffix_offset(&b.path, path).expect("a cached binding still prefix-matches its cached path");
            return Some((resolve_target(&b.target), b.rights, &path[off..]));
        }

        let mut best: Option<(usize, usize)> = None; // (binding index, suffix offset)
        for (i, b) in guard.bindings.iter().enumerate() {
            let Some(off) = match_suffix_offset(&b.path, path) else {
                continue;
            };
            match best {
                Some((bi, _)) if guard.bindings[bi].path.len() >= b.path.len() => {}
                _ => best = Some((i, off)),
            }
        }
        let (i, off) = best?;
        let result = {
            let b = &guard.bindings[i];
            (resolve_target(&b.target), b.rights, &path[off..])
        };
        // Insert into the cache (best-effort): round-robin evict when full; a failed path copy just
        // skips caching. No `ObjectRef` is stored, so nothing is dropped under the lock.
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

    #[cfg(test)]
    pub(crate) fn cache_len(&self) -> usize {
        self.inner.lock().cache.len()
    }

    /// Fill `out` with the `index`-th binding (insertion order): path (truncated to
    /// [`NS_ENTRY_PATH_MAX`], true length in `path_len`), target `kind` and `rights`; `false` past
    /// the count. For `sys_ns_enumerate`, not a filesystem `readdir`. No allocation under the lock.
    pub fn enumerate(&self, index: usize, out: &mut NsEntry) -> bool {
        let g = self.inner.lock();
        let Some(b) = g.bindings.get(index) else {
            return false;
        };
        let true_len = b.path.len();
        let n = true_len.min(NS_ENTRY_PATH_MAX);
        out.path[..n].copy_from_slice(&b.path[..n]);
        out.path_len = true_len as u32;
        out.kind = match &b.target {
            BindingTarget::DirectHandle(_) => NS_KIND_DIRECT,
            BindingTarget::KernelServer(_) => NS_KIND_KERNEL,
            BindingTarget::UserspaceServer(_, _) => NS_KIND_MOUNT,
        };
        out.rights = b.rights.bits();
        true
    }

    /// A new namespace holding **a copy of every binding** in this one — `sys_ns_derive`.
    ///
    /// **A snapshot, not a view**: the copy owns its binding list, so a later bind or unbind in
    /// either namespace does not reach the other. *Targets* are shared, not duplicated: a direct
    /// handle's object and a userspace server's registration each gain a reference, and a kernel
    /// server's id is copied. Subtree bases and rights are copied; the resolution cache is not.
    ///
    /// **The module's lock discipline, twice over.** Targets are cloned under this namespace's
    /// lock (an atomic bump, never a drop); the copy is built in a list that outlives the guard, so
    /// a failed allocation partway drops the targets cloned so far only *after* the lock is
    /// released. The new namespace's lock is taken only to install the finished list, never while
    /// this one's is held: both are rank 4.
    ///
    /// A view is its caller's namespace plus a profile's grants
    /// (`docs/planning/administration.md` § *A view is your namespace plus a profile*).
    pub fn try_derive(&self) -> Result<KBox<Namespace>, AllocError> {
        let derived = Namespace::try_new()?;
        let mut copied: KVec<Binding> = KVec::new();
        let copy = {
            let guard = self.inner.lock();
            copy_bindings(&guard.bindings, &mut copied)
        };
        copy?;
        // Swap rather than assign, so the empty list it replaces is dropped outside the new lock.
        core::mem::swap(&mut derived.inner.lock().bindings, &mut copied);
        Ok(derived)
    }
}

// No `Drop` impl: the `KBox` drop (run by `dispatch_destroy`, outside any lock) drops `inner`
// and so each `Binding`'s target, releasing every bound resource.

/// Validate a namespace path: non-empty, absolute, ≤ [`NS_PATH_MAX`], and — except for root —
/// every component non-empty and not `.`/`..` (the kernel never interprets them), no trailing `/`.
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

/// Project a stored [`BindingTarget`] into a [`ResolvedTarget`] **under the namespace lock**: a
/// handle is `clone`d (an atomic bump), a kernel server's id copied; neither drops an `ObjectRef`.
fn resolve_target(t: &BindingTarget) -> ResolvedTarget {
    match t {
        BindingTarget::DirectHandle(obj) => ResolvedTarget::DirectHandle(obj.clone()),
        BindingTarget::KernelServer(id) => ResolvedTarget::KernelServer(*id),
        BindingTarget::UserspaceServer(reg, base) => ResolvedTarget::UserspaceServer(reg.clone(), *base),
    }
}

/// Copy every binding in `from` onto the end of `into`, for [`Namespace::try_derive`]: targets
/// shared (handle or registration cloned, kernel-server id copied), bases and rights as they are.
///
/// **Called under the source's lock**: it allocates and clones, and never drops a target. An
/// `Err` leaves `into` holding the bindings copied so far, for the caller to drop once the lock
/// is released.
fn copy_bindings(from: &KVec<Binding>, into: &mut KVec<Binding>) -> Result<(), AllocError> {
    into.try_reserve(from.len())?;
    for b in from.iter() {
        let mut path: KVec<u8> = KVec::new();
        path.try_extend_from_slice(&b.path)?;
        let target = match &b.target {
            BindingTarget::DirectHandle(obj) => BindingTarget::DirectHandle(obj.clone()),
            BindingTarget::KernelServer(id) => BindingTarget::KernelServer(*id),
            BindingTarget::UserspaceServer(reg, base) => BindingTarget::UserspaceServer(reg.clone(), *base),
        };
        into.try_push(Binding { path, target, rights: b.rights }).expect("slot reserved above");
    }
    Ok(())
}

/// If `binding` is a component-boundary prefix of the absolute `query`, the byte offset where
/// the suffix begins (past any boundary `/`); `None` otherwise. Root matches every absolute query.
fn match_suffix_offset(binding: &[u8], query: &[u8]) -> Option<usize> {
    if binding == b"/" {
        return Some(1.min(query.len()));
    }
    if query.len() < binding.len() || &query[..binding.len()] != binding {
        return None;
    }
    if query.len() == binding.len() {
        return Some(query.len()); // exact match → empty suffix
    }
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

    fn target() -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut (), KObjectType::Timer)
        }
    }

    fn ns() -> KBox<Namespace> {
        init_global_heap();
        Namespace::try_new().unwrap()
    }

    #[test]
    fn enumerate_lists_bindings_in_order_with_kind() {
        use crate::object::kernel_server::KernelServerId;
        let n = ns();
        n.bind(b"/store", target(), Rights::LOOKUP | Rights::INSPECT).unwrap();
        n.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, Rights::READ).unwrap();

        let mut e = NsEntry::zeroed();
        assert!(n.enumerate(0, &mut e));
        assert_eq!(&e.path[..e.path_len as usize], b"/store");
        assert_eq!(e.kind, NS_KIND_DIRECT);
        assert_eq!(e.rights, (Rights::LOOKUP | Rights::INSPECT).bits());

        assert!(n.enumerate(1, &mut e));
        assert_eq!(&e.path[..e.path_len as usize], b"/dev/entropy");
        assert_eq!(e.kind, NS_KIND_KERNEL);

        assert!(!n.enumerate(2, &mut e));
    }

    #[test]
    fn validate_path_accepts_and_rejects() {
        assert!(validate_path(b"/").is_ok());
        assert!(validate_path(b"/dev").is_ok());
        assert!(validate_path(b"/dev/log").is_ok());
        for bad in [
            &b""[..],
            b"dev",   // relative
            b"/dev/", // trailing slash
            b"//dev", // empty component
            b"/./x",  // dot
            b"/../x", // dotdot
            b"/a//b", // empty interior component
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
        let (t, e) = n.bind(b"bad", target(), Rights::LOOKUP).unwrap_err();
        assert_eq!(e, NsError::InvalidPath);
        drop(t);
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
        assert!(n.bind(b"/dev", target(), Rights::LOOKUP).is_ok());
    }

    #[test]
    fn resolve_exact_prefix_and_longest_match() {
        let n = ns();
        n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
        n.bind(b"/dev/log", target(), Rights::LOOKUP | Rights::INSPECT).unwrap();
        n.bind(b"/store", target(), Rights::LOOKUP).unwrap();

        let (_, _, suf) = n.resolve(b"/store").unwrap();
        assert_eq!(suf, b"");
        let (_, _, suf) = n.resolve(b"/dev/tty0").unwrap();
        assert_eq!(suf, b"tty0");
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

        let (_, r0, s0) = n.resolve(b"/dev/log").unwrap();
        assert_eq!(n.cache_len(), 1);
        let (_, r1, s1) = n.resolve(b"/dev/log").unwrap();
        assert_eq!(n.cache_len(), 1, "repeat lookup reuses the entry");
        assert_eq!((r0, s0), (r1, s1), "cache hit matches the cold resolve");

        let (_, _, suf) = n.resolve(b"/dev/tty0").unwrap();
        assert_eq!(suf, b"tty0");
        assert_eq!(n.cache_len(), 2);

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
        assert!(n.resolve(b"/devices").is_none());
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
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let r = unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(Namespace::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::Namespace,
            )
        };
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

    #[test]
    fn bind_kernel_server_resolves_to_the_id() {
        let n = ns();
        n.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, Rights::READ).unwrap();
        let (target, rights, suf) = n.resolve(b"/dev/entropy").unwrap();
        assert_eq!(suf, b"");
        assert_eq!(rights, Rights::READ);
        match target {
            ResolvedTarget::KernelServer(id) => assert_eq!(id, KernelServerId::Entropy),
            ResolvedTarget::DirectHandle(_) | ResolvedTarget::UserspaceServer(..) => {
                panic!("expected a kernel-server target")
            }
        }
        let (target, _, suf) = n.resolve(b"/dev/entropy/x").unwrap();
        assert_eq!(suf, b"x");
        assert!(matches!(target, ResolvedTarget::KernelServer(_)));
    }

    #[test]
    fn bind_kernel_server_rejects_invalid_and_duplicate() {
        let n = ns();
        assert_eq!(n.bind_kernel_server(b"bad", KernelServerId::Entropy, Rights::READ), Err(NsError::InvalidPath));
        n.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, Rights::READ).unwrap();
        assert_eq!(
            n.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, Rights::READ),
            Err(NsError::AlreadyBound)
        );
    }

    #[test]
    fn direct_and_kernel_server_bindings_coexist() {
        let n = ns();
        n.bind(b"/dev/log", target(), Rights::LOOKUP).unwrap();
        n.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, Rights::READ).unwrap();
        assert!(matches!(n.resolve(b"/dev/log").unwrap().0, ResolvedTarget::DirectHandle(_)));
        assert!(matches!(n.resolve(b"/dev/entropy").unwrap().0, ResolvedTarget::KernelServer(_)));
    }

    fn us_reg_target() -> ObjectRef {
        use crate::object::{IpcChannel, UserspaceServerReg};
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        // SAFETY: adopt the kernel endpoint's creation reference; drop the peer's.
        let endpoint = unsafe { ObjectRef::from_raw(KBox::into_raw(a).as_ptr() as *mut (), KObjectType::IpcChannel) };
        // SAFETY: adopts the peer's creation reference, to drop it.
        drop(unsafe { ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), KObjectType::IpcChannel) });
        let reg = UserspaceServerReg::try_new(endpoint).unwrap();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(KBox::into_raw(reg).as_ptr() as *mut (), KObjectType::UserspaceServerReg) }
    }

    #[test]
    fn bind_userspace_server_resolves_to_the_registration() {
        let n = ns();
        n.bind_userspace_server(b"/fs", us_reg_target(), Rights::LOOKUP | Rights::MAP_READ, SubtreeBase::empty())
            .unwrap();
        let (target, rights, suf) = n.resolve(b"/fs").unwrap();
        assert_eq!(suf, b"");
        assert_eq!(rights, Rights::LOOKUP | Rights::MAP_READ);
        match target {
            ResolvedTarget::UserspaceServer(reg, base) => {
                assert_eq!(reg.object_type(), KObjectType::UserspaceServerReg);
                assert!(base.is_empty(), "an unscoped mount carries no base");
            }
            _ => panic!("expected a userspace-server target"),
        }
        let (target, _, suf) = n.resolve(b"/fs/system/current-generation").unwrap();
        assert_eq!(suf, b"system/current-generation");
        assert!(matches!(target, ResolvedTarget::UserspaceServer(..)));
    }

    #[test]
    fn subtree_base_from_path_validates() {
        let b = SubtreeBase::from_path(b"/home/alice").unwrap();
        assert_eq!(b.as_path(), b"/home/alice");
        assert!(!b.is_empty());
        assert!(SubtreeBase::empty().is_empty());
        for bad in [&b"home"[..], b"/a/../b", b"/./x", b"/", b"//x"] {
            assert_eq!(SubtreeBase::from_path(bad), Err(NsError::InvalidPath), "{bad:?}");
        }
        let toolong = {
            let mut v = [b'a'; NS_SUBTREE_BASE_MAX + 2];
            v[0] = b'/';
            v
        };
        assert_eq!(SubtreeBase::from_path(&toolong), Err(NsError::InvalidPath));
    }

    #[test]
    fn bind_userspace_server_carries_subtree_base() {
        let n = ns();
        let base = SubtreeBase::from_path(b"/home/alice").unwrap();
        n.bind_userspace_server(b"/home", us_reg_target(), Rights::LOOKUP | Rights::WRITE, base).unwrap();
        let (target, _, suf) = n.resolve(b"/home/notes.txt").unwrap();
        assert_eq!(suf, b"notes.txt");
        match target {
            ResolvedTarget::UserspaceServer(_, b) => assert_eq!(b.as_path(), b"/home/alice"),
            _ => panic!("expected a userspace-server target"),
        }
    }

    /// **One registration can be bound at two paths, and the two coexist**, one unscoped and one
    /// subtree-scoped, each with its own suffix and base: the shape `init` and `session-mgr` rely
    /// on. The same-pointer assertion holds by construction; each binding's own base is the real
    /// check. Sharing is pinned end to end by `boot-probe`'s `subtree_bind_test`.
    #[test]
    fn one_registration_can_be_bound_at_two_paths() {
        let n = ns();
        let reg = us_reg_target();
        let same = reg.clone();
        let addr = reg.as_ptr() as usize;
        n.bind_userspace_server(b"/", reg, Rights::LOOKUP, SubtreeBase::empty()).unwrap();
        let base = SubtreeBase::from_path(b"/system").unwrap();
        n.bind_userspace_server(b"/subtreetest", same, Rights::LOOKUP, base).unwrap();

        let (direct, _, dsuf) = n.resolve(b"/system/current-generation").unwrap();
        assert_eq!(dsuf, b"system/current-generation");
        let (scoped, _, ssuf) = n.resolve(b"/subtreetest/current-generation").unwrap();
        assert_eq!(ssuf, b"current-generation");

        match (direct, scoped) {
            (ResolvedTarget::UserspaceServer(a, abase), ResolvedTarget::UserspaceServer(b, bbase)) => {
                assert_eq!(a.as_ptr() as usize, addr);
                assert_eq!(b.as_ptr() as usize, addr);
                assert!(abase.is_empty(), "the unscoped binding must carry no base");
                assert_eq!(bbase.as_path(), b"/system");
            }
            _ => panic!("expected userspace-server targets"),
        }
    }

    #[test]
    fn bind_userspace_server_hands_back_target_on_error() {
        let n = ns();
        let (t, e) =
            n.bind_userspace_server(b"bad", us_reg_target(), Rights::LOOKUP, SubtreeBase::empty()).unwrap_err();
        assert_eq!(e, NsError::InvalidPath);
        assert_eq!(t.object_type(), KObjectType::UserspaceServerReg);
        drop(t);
        n.bind_userspace_server(b"/fs", us_reg_target(), Rights::LOOKUP, SubtreeBase::empty()).unwrap();
        let (t, e) =
            n.bind_userspace_server(b"/fs", us_reg_target(), Rights::LOOKUP, SubtreeBase::empty()).unwrap_err();
        assert_eq!(e, NsError::AlreadyBound);
        drop(t);
    }

    #[test]
    fn unbind_userspace_server_returns_the_registration_ref() {
        let n = ns();
        n.bind_userspace_server(b"/fs", us_reg_target(), Rights::LOOKUP, SubtreeBase::empty()).unwrap();
        match n.unbind(b"/fs").expect("was bound") {
            BindingTarget::UserspaceServer(reg, _) => {
                assert_eq!(reg.object_type(), KObjectType::UserspaceServerReg);
                drop(reg); // released outside the lock
            }
            _ => panic!("expected a userspace-server target"),
        }
        assert!(n.unbind(b"/fs").is_none(), "already removed");
    }

    #[test]
    fn unbind_kernel_server_returns_drop_free_target() {
        let n = ns();
        n.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, Rights::READ).unwrap();
        match n.unbind(b"/dev/entropy").expect("was bound") {
            BindingTarget::KernelServer(id) => assert_eq!(id, KernelServerId::Entropy),
            BindingTarget::DirectHandle(_) | BindingTarget::UserspaceServer(..) => {
                panic!("expected a kernel-server target")
            }
        }
        assert!(n.unbind(b"/dev/entropy").is_none(), "already removed");
    }

    /// **A copy resolves every kind of binding the way its source does**, with the same subtree
    /// base and rights: a copy that lost a base would widen a session's `/home` to the whole
    /// filesystem.
    #[test]
    fn derive_copies_every_binding_kind_with_its_rights_and_base() {
        let n = ns();
        let obj = target();
        let obj_addr = obj.as_ptr() as usize;
        n.bind(b"/dev/timer", obj, Rights::LOOKUP | Rights::INSPECT).unwrap();
        n.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, Rights::READ).unwrap();
        let reg = us_reg_target();
        let reg_addr = reg.as_ptr() as usize;
        let base = SubtreeBase::from_path(b"/home/alice").unwrap();
        n.bind_userspace_server(b"/home", reg, Rights::LOOKUP | Rights::WRITE, base).unwrap();

        let d = n.try_derive().unwrap();
        let (t, r, suf) = d.resolve(b"/dev/timer").unwrap();
        assert_eq!((r, suf), (Rights::LOOKUP | Rights::INSPECT, &b""[..]));
        match t {
            ResolvedTarget::DirectHandle(o) => assert_eq!(o.as_ptr() as usize, obj_addr),
            _ => panic!("expected a direct handle"),
        }
        let (t, r, _) = d.resolve(b"/dev/entropy").unwrap();
        assert_eq!(r, Rights::READ);
        assert!(matches!(t, ResolvedTarget::KernelServer(KernelServerId::Entropy)));
        let (t, r, suf) = d.resolve(b"/home/notes.txt").unwrap();
        assert_eq!((r, suf), (Rights::LOOKUP | Rights::WRITE, &b"notes.txt"[..]));
        match t {
            ResolvedTarget::UserspaceServer(reg, b) => {
                assert_eq!(reg.as_ptr() as usize, reg_addr, "the same registration, shared");
                assert_eq!(b.as_path(), b"/home/alice", "the base came with the binding");
            }
            _ => panic!("expected a userspace-server target"),
        }
    }

    /// **A snapshot, not a view, in both directions.**
    #[test]
    fn derive_is_a_snapshot_in_both_directions() {
        let n = ns();
        n.bind(b"/a", target(), Rights::LOOKUP).unwrap();
        n.bind(b"/b", target(), Rights::LOOKUP).unwrap();
        let d = n.try_derive().unwrap();

        n.bind(b"/c", target(), Rights::LOOKUP).unwrap();
        drop(n.unbind(b"/a").expect("was bound"));
        assert!(d.resolve(b"/c").is_none(), "a later bind in the source reached the copy");
        assert!(d.resolve(b"/a").is_some(), "an unbind in the source reached the copy");

        d.bind(b"/d", target(), Rights::LOOKUP).unwrap();
        drop(d.unbind(b"/b").expect("was bound"));
        assert!(n.resolve(b"/d").is_none(), "a bind in the copy reached the source");
        assert!(n.resolve(b"/b").is_some(), "an unbind in the copy reached the source");
    }

    /// **Targets are shared, not moved or duplicated**: a reference too few would free a
    /// registration under a live binding, one too many would leak it.
    #[test]
    fn derive_shares_targets_and_each_is_freed_once() {
        init_global_heap();
        test_probe::reset();
        let n = Namespace::try_new().unwrap();
        n.bind(b"/a", target(), Rights::LOOKUP).unwrap();
        n.bind(b"/b", target(), Rights::LOOKUP).unwrap();
        n.bind_userspace_server(b"/fs", us_reg_target(), Rights::LOOKUP, SubtreeBase::empty()).unwrap();
        let d = n.try_derive().unwrap();

        drop(n);
        assert_eq!(test_probe::timer_destroys(), 0, "the copy still binds both timers");
        assert_eq!(test_probe::userspace_server_reg_destroys(), 0, "and the registration");
        assert!(d.resolve(b"/a").is_some());
        drop(d);
        assert_eq!(test_probe::timer_destroys(), 2, "each timer freed once");
        assert_eq!(test_probe::userspace_server_reg_destroys(), 1, "the registration freed once");
    }

    /// **Unbinding can widen.** Removing a narrower binding hands its paths to the broader one
    /// beneath it: the source confines `/home/bob/x` to alice's subtree, and a copy with `/home`
    /// unbound reaches bob's. **A narrower binding must never hide part of a broader one.**
    #[test]
    fn unbinding_a_narrower_binding_in_a_copy_exposes_the_broader_one() {
        let n = ns();
        let whole = us_reg_target();
        let whole_addr = whole.as_ptr() as usize;
        n.bind_userspace_server(b"/", whole, Rights::LOOKUP, SubtreeBase::empty()).unwrap();
        let alice = SubtreeBase::from_path(b"/home/alice").unwrap();
        n.bind_userspace_server(b"/home", us_reg_target(), Rights::LOOKUP, alice).unwrap();

        let d = n.try_derive().unwrap();
        drop(d.unbind(b"/home").expect("was bound"));
        match d.resolve(b"/home/bob/x").unwrap() {
            (ResolvedTarget::UserspaceServer(reg, b), _, suf) => {
                assert_eq!(reg.as_ptr() as usize, whole_addr, "the whole filesystem");
                assert_eq!((b.as_path(), suf), (&b""[..], &b"home/bob/x"[..]), "bob's file");
            }
            _ => panic!("expected a userspace-server target"),
        }
        match n.resolve(b"/home/bob/x").unwrap() {
            (ResolvedTarget::UserspaceServer(_, b), _, suf) => {
                assert_eq!((b.as_path(), suf), (&b"/home/alice"[..], &b"bob/x"[..]), "confined");
            }
            _ => panic!("expected a userspace-server target"),
        }
    }

    /// **The copy's cache starts empty**: inherited entries index *the source's* binding list.
    #[test]
    fn derive_starts_with_an_empty_cache() {
        let n = ns();
        n.bind(b"/dev", target(), Rights::LOOKUP).unwrap();
        let _ = n.resolve(b"/dev/tty0");
        assert_eq!(n.cache_len(), 1, "precondition: the source has a cached entry");
        let d = n.try_derive().unwrap();
        assert_eq!(d.cache_len(), 0);
        assert!(d.resolve(b"/dev/tty0").is_some());
        assert_eq!(d.cache_len(), 1, "and it caches its own resolutions");
    }
}
