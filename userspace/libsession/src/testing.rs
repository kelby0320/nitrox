//! What the host tests share: endpoints that say which they are, and a namespace's bindings read
//! back.

use libkern::{RawHandle, Rights};
use libos::{CanLookup, Channel, Handle, Namespace, NsMutable, NsReadOnly, Only, Ref, mock};
use std::string::String;
use std::sync::{Mutex, MutexGuard};
use std::vec::Vec;

/// An endpoint whose rights are exactly `1 << tag`, so a binding of it says which one it was, and
/// its peer, which keeps it from reading as closed.
pub(crate) fn endpoint(tag: u32) -> (Handle<Channel, Only>, Handle<Channel, Only>) {
    let (ours, peer) = Handle::<Channel, Only>::pair(4).unwrap();
    let tagged = mock::with_rights(ours.raw().0, Rights::from_bits(1 << tag));
    // SAFETY: a handle the mock has just made, which nothing else holds.
    let tagged = unsafe { Handle::from_raw(RawHandle(tagged), Rights::empty()) };
    (tagged, peer)
}

/// `ns`'s bindings as `(path, rights)`, in the order they were made.
pub(crate) fn entries<M: CanLookup>(ns: Ref<'_, Namespace, M>) -> Vec<(String, u64)> {
    ns.bindings()
        .map(|e| {
            let e = e.unwrap();
            (String::from_utf8(e.path[..e.path_len as usize].to_vec()).unwrap(), e.rights)
        })
        .collect()
}

/// `ns` as a namespace to look up in.
pub(crate) fn lookup_only(ns: &Handle<Namespace, NsMutable>) -> Ref<'_, Namespace, NsReadOnly> {
    // SAFETY: the borrow lasts as long as `ns`, which stays open.
    unsafe { Ref::from_raw(ns.raw(), Rights::empty()) }
}

/// Held by a test that sets or reads whether a stop was asked, which is one flag for the process.
pub(crate) fn stop_flag() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::STOP_ASKED.store(false, core::sync::atomic::Ordering::Relaxed);
    held
}
