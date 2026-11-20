//! **The routes**: this manager's own endpoints, one per server path and derived endpoint, each
//! answered `SUBNAMESPACE` into the registry at one place, so a restart rebinds there alone.

use alloc::string::String;
use alloc::vec::Vec;

use libkern::{KError, Rights};
use libos::log::Line;
use libos::{Channel, Deadline, Handle, Msg, Namespace, Notify, NsMutable, Only, WaitSet};
use librsproto::server;

use crate::registry;

/// One of this manager's channel ends.
pub type Endpoint = Handle<Channel, Only>;
/// A namespace this manager binds in: the root with `init`'s rights, or its registry.
pub type BindNs = Handle<Namespace, NsMutable>;

/// How deep each route is: a full ring answers the resolver `WouldBlock` at once.
pub const SERVE_DEPTH: u32 = 64;

/// The most requests a pass takes from one channel, so a client that keeps its ring full cannot
/// hold this manager from its other work: a ring left non-empty signals the next wait at once.
pub const PER_PASS: usize = 16;

/// **A route**, reaching **one** server: one that reached any server by the suffix could not be
/// handed to a session, since `desktop-shell` could bind it at a base of its choosing.
pub struct Route {
    pub svc: usize,
    /// `None` for the server's own path; `Some(suffix)` for an endpoint it mints for sessions.
    pub through: Option<&'static str>,
    /// Where in the registry each resolve continues: `/<name>` or `/<name>.<suffix>`.
    pub base: String,
    pub client: Endpoint,
    pub serve: Endpoint,
    /// A derived route's endpoint as the server minted it, bound at `base`.
    pub target: Option<Endpoint>,
    /// Whether something is bound at `base`; else a resolve is `NotFound`.
    pub live: bool,
}

/// Every route, kept for the boot, by its index.
#[derive(Default)]
pub struct Routes(pub Vec<Route>);

impl core::ops::Index<usize> for Routes {
    type Output = Route;
    fn index(&self, r: usize) -> &Route {
        &self.0[r]
    }
}

impl core::ops::IndexMut<usize> for Routes {
    fn index_mut(&mut self, r: usize) -> &mut Route {
        &mut self.0[r]
    }
}

impl Routes {
    /// The route for server `svc`, `name`, its own path or a derived endpoint, made when first
    /// needed. `None` when there is no room, which is said.
    pub fn route(&mut self, svc: usize, name: &str, through: Option<&'static str>) -> Option<usize> {
        if let Some(r) = self.0.iter().position(|r| r.svc == svc && r.through == through) {
            return Some(r);
        }
        let room = self.0.len() < registry::MAX_ROUTES;
        let made = if room { Handle::pair(SERVE_DEPTH).ok() } else { None };
        let Some((client, serve)) = made else {
            let why: &[u8] = if room { b"channel create FAIL" } else { b"all in use" };
            Line::new().s(b"service-mgr: no route for '").s(name.as_bytes()).s(b"' -- ").s(why).end();
            return None;
        };
        let base = match through {
            None => registry::base(name),
            Some(t) => registry::derived(name, t),
        };
        self.0.push(Route { svc, through, base, client, serve, target: None, live: false });
        Some(self.0.len() - 1)
    }

    /// A copy of the route to server `svc` for a login supervisor, while something is bound there.
    pub fn live_copy(&self, svc: usize, through: Option<&str>) -> Option<Endpoint> {
        let found = self.0.iter().find(|r| r.live && r.through == through && r.svc == svc);
        found.and_then(|r| r.client.duplicate(Rights::TRANSFER | Rights::DUPLICATE).ok())
    }

    /// Add every route to `set`, telling `named` each one's index there and its own.
    pub fn watch<'a, const N: usize>(&'a self, set: &mut WaitSet<'a, N>, mut named: impl FnMut(usize, usize)) {
        for (r, route) in self.0.iter().enumerate() {
            if let Ok(i) = set.add(&route.serve) {
                named(i, r);
            }
        }
    }

    /// Answer every resolve queued on each route `pick` names: `SUBNAMESPACE` into `registry` at
    /// the route's base while something is bound there, `NotFound` while not.
    pub fn serve(&self, registry: Option<&BindNs>, pick: impl Fn(usize) -> bool) {
        for (_, r) in self.0.iter().enumerate().filter(|&(k, _)| pick(k)) {
            for (op, request_id, body) in take(&r.serve).0 {
                let resolve = librsproto::OP_NS_RESOLVE;
                if op != resolve {
                    server::reply_error(&r.serve, op, request_id, KError::Unsupported, b"");
                } else if librsproto::namespace::parse_resolve_request(&body).is_none() {
                    server::reply_error(&r.serve, resolve, request_id, KError::InvalidArgument, b"");
                } else if let (true, Some(registry)) = (r.live, registry) {
                    server::reply_subnamespace(&r.serve, request_id, registry.as_ref(), 0, r.base.as_bytes());
                } else {
                    server::reply_error(&r.serve, resolve, request_id, KError::NotFound, b"");
                }
            }
        }
    }
}

/// Wait for `notif` until `deadline`, answering the routes meanwhile: whether `notif` signalled. A
/// route's wake does not end it, as the code it waits for may come next.
pub fn wait_codes(notif: &Handle<Notify, Only>, routes: &Routes, registry: Option<&BindNs>, deadline: u64) -> bool {
    const N: usize = libkern::abi::MAX_WAIT_HANDLES;
    let mut route_at = [usize::MAX; N];
    let mut set = WaitSet::<N>::new();
    let Ok(notif_at) = set.add(notif) else { return false };
    routes.watch(&mut set, |i, r| route_at[i] = r);
    loop {
        let Ok(waited) = set.wait(Deadline::at(deadline)) else {
            return false;
        };
        let fired: Vec<usize> = waited.iter().map(|(i, _)| i).collect();
        routes.serve(registry, |r| fired.iter().any(|&i| route_at[i] == r));
        if fired.contains(&notif_at) {
            return true;
        }
        // Nothing signalled: the deadline passed. A route that keeps signalling ends it by the clock.
        if fired.is_empty() || libos::time::now().is_ok_and(|now| now >= deadline) {
            return false;
        }
    }
}

/// Up to [`PER_PASS`] requests from `ch`, and whether its peer has gone with none left.
pub fn take(ch: &Endpoint) -> (Vec<(u16, u64, Vec<u8>)>, bool) {
    let mut taken = Vec::new();
    while taken.len() < PER_PASS {
        match recv_request(ch) {
            Ok(Some(request)) => taken.push(request),
            Ok(None) => return (taken, false),
            Err(()) => return (taken, true),
        }
    }
    (taken, false)
}

/// One message on `ch`, its handles closed: `Err(())` once **a peer that has gone** is all that is
/// left, as a closed channel stays signalled and a caller must drop it or the wait loop would
/// spin; `None` when none is queued. A route's cannot close.
#[allow(clippy::result_unit_err, reason = "its one error is the peer gone, which each caller drops alike")]
pub fn recv_request(ch: &Endpoint) -> Result<Option<(u16, u64, Vec<u8>)>, ()> {
    let mut msg = Msg::new();
    loop {
        let _sent = match ch.recv(&mut msg) {
            Ok(sent) => sent,
            Err(e) if e.code() == KError::PeerClosed => return Err(()),
            Err(_) => return Ok(None),
        };
        if let Ok(m) = librsproto::decode(msg.payload()) {
            return Ok(Some((m.op, m.request_id, m.body.to_vec())));
        }
    }
}

#[cfg(test)]
mod tests;
