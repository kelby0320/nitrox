//! The shared session core: **authenticate → construct the namespace → spawn the leader →
//! reap → tear down**.
//!
//! `session-mgr` and `desktop-session-mgr` run the same sequence against different arguments: a
//! **shared library, not a merged process** (`docs/architecture/graphical-session.md` §4).
//! **The greeter is not here**; it starts with credentials in hand and ends with a leader that
//! has exited. `no_std` with `alloc`, kept minimal as `session-mgr` links it (its `CLAUDE.md`).

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

mod auth;
mod blocks;
mod leader;
mod namespace;
#[cfg(test)]
mod testing;

use core::sync::atomic::{AtomicBool, Ordering};
use libkern::{KIND_TERMINATE_REQUESTED, Rights};
use libos::{CanLookup, Handle, Namespace, Notify, Only, Ref, block_on};

pub use auth::{authenticate_on, close_views_session, open_views_session};
pub use blocks::{share_block_devices, withdraw_block_devices};
pub use leader::run_leader;
pub use namespace::{Bound, Constructed, Members, build_namespace};

/// Whether a terminate request has reached this process; set by the leader's wait and the drain.
static STOP_ASKED: AtomicBool = AtomicBool::new(false);

/// How long a leader has, once asked to stop, before its supervisor goes on without it; under
/// `service-mgr`'s 10 s bound. **Public, because a leader's own wait must fit inside it.**
pub const LEADER_STOP_NS: u64 = 5_000_000_000;

/// **Whether this process has been asked to stop**: a supervisor exits rather than prompting again.
pub fn stop_asked() -> bool {
    STOP_ASKED.load(Ordering::Relaxed)
}

/// Drain `notif`, saying whether a terminate request was among it; for a supervisor between
/// sessions, with no child whose `ChildExited` could be dropped.
pub fn drain_stop_requests<'a>(notif: impl Into<Ref<'a, Notify, Only>>) -> bool {
    let notif = notif.into();
    while let Ok(Some(n)) = notif.recv() {
        if n.kind == KIND_TERMINATE_REQUESTED {
            STOP_ASKED.store(true, Ordering::Relaxed);
        }
    }
    stop_asked()
}

/// Resolve `path` in `ns` with `rights`, waiting the PO.
pub fn resolve<'a, T, M2, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    rights: Rights,
) -> libos::Result<Handle<T, M2>> {
    block_on(ns.into().lookup(path, rights))
}

/// The user's home *as seen from inside the session*; the outside path is not nameable.
const SESSION_HOME: &str = "/home";

/// Build the session's environment, a TSM1 `Record`. **`USER` is deliberately absent**: identity
/// lives at `/session/user`, which a process cannot forge; an env var it could tell a child.
pub fn session_env() -> libstream::wire::Record {
    use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};
    let schema = Schema::new()
        .field("HOME", TypeTag::String, TypeModifiers::NONE)
        .field("PWD", TypeTag::String, TypeModifiers::NONE)
        .field("PATH", TypeTag::List, TypeModifiers::NONE);
    let path: alloc::vec::Vec<Value> = alloc::vec![Value::Str(alloc::string::String::from("/bin"))];
    Record {
        schema,
        values: alloc::vec![
            Value::Str(alloc::string::String::from(SESSION_HOME)),
            Value::Str(alloc::string::String::from(SESSION_HOME)),
            Value::List(alloc::sync::Arc::from(path)),
        ],
    }
}

#[cfg(test)]
mod tests;
