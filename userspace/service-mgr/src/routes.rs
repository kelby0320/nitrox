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

/// The most messages a pass takes from one channel, requests or not, so a client that keeps its ring
/// full cannot hold this manager from its other work: a ring left non-empty signals the next wait.
pub const PER_PASS: usize = 16;

/// How deep a control channel is: shallower than [`PER_PASS`], so one look finds a peer gone.
pub const CONTROL_DEPTH: u32 = 10;

/// **A route**, reaching **one** server: one that reached any server by the suffix could not be
/// handed to a session, since `desktop-shell` could bind it at a base of its choosing.
pub struct Route {
    /// Its server's index among the declarations.
    pub svc: usize,
    /// `None` for the server's own path; `Some(suffix)` for an endpoint it mints for sessions.
    pub through: Option<&'static str>,
    /// Where in the registry each resolve continues: `/<name>` or `/<name>.<suffix>`.
    pub base: String,
    /// The end a resolve reaches: bound at the server's path, and copied to the login supervisors.
    pub client: Endpoint,
    /// The end this manager answers on.
    pub serve: Endpoint,
    /// A derived route's endpoint as the server minted it, bound at `base`.
    pub target: Option<Endpoint>,
    /// Whether something is bound at `base`; else a resolve is `NotFound`.
    pub live: bool,
}

/// Every route, kept for the boot, by its index.
#[derive(Default)]
pub struct Routes(
    /// Each route, at the index [`Routes::route`] gave it.
    pub Vec<Route>,
);

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

    /// A copy of the route to server `svc`, `name`, made if need be: `NotFound` until it is up.
    pub fn copy(&mut self, svc: usize, name: &str, through: Option<&'static str>) -> Option<Endpoint> {
        let r = self.route(svc, name, through)?;
        self.0[r].client.duplicate(Rights::TRANSFER | Rights::DUPLICATE).ok()
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

/// The requests among up to [`PER_PASS`] messages on `ch`, and whether its peer went, none left.
pub fn take(ch: &Endpoint) -> (Vec<(u16, u64, Vec<u8>)>, bool) {
    let (mut msg, mut taken) = (Msg::new(), Vec::new());
    for _ in 0..PER_PASS {
        match ch.recv(&mut msg) {
            Ok(_sent) => {
                taken.extend(librsproto::decode(msg.payload()).ok().map(|m| (m.op, m.request_id, m.body.to_vec())))
            }
            Err(e) if e.code() == KError::PeerClosed => return (taken, true),
            Err(_) => return (taken, false),
        }
    }
    (taken, false)
}

/// **Serve each channel in `list` that `pick` names**: `each`, which must leave `list` as it is, is
/// handed what it asked and its index; one whose peer has gone is then taken out, `gone` told.
pub fn serve_each<M>(
    m: &mut M,
    list: fn(&mut M) -> &mut Vec<Endpoint>,
    pick: impl Fn(u64) -> bool,
    mut each: impl FnMut(&mut M, usize, (u16, u64, Vec<u8>)),
    mut gone: impl FnMut(&mut M, u64),
) {
    let mut k = 0;
    while k < list(m).len() {
        let id = list(m)[k].raw().0;
        let (asked, closed) = if pick(id) { take(&list(m)[k]) } else { (Vec::new(), false) };
        for request in asked {
            each(m, k, request);
        }
        if closed {
            list(m).remove(k);
            gone(m, id);
        } else {
            k += 1;
        }
    }
}

/// Whether `ch`'s peer has gone: drain the endpoint until it answers.
///
/// `sys_channel_recv` answers `WouldBlock` when the ring is merely empty and the peer is alive,
/// `PeerClosed` when it is empty and the peer is gone; that is what makes a control channel an exit
/// discriminator (see `Mgr::poll`). **A drain, not a single receive**, because a receive that
/// returns `0` has *consumed* a message: a queued message would otherwise mask the close behind it.
/// A service that is not a server cannot send here — its control end is granted `RECV | WAIT` and
/// no `SEND` — but **a server can**, and this is where a `Ready` lands that came after its
/// deadline.
///
/// **At most [`PER_PASS`] messages**: a peer gone left no more than [`CONTROL_DEPTH`], and one still
/// sending is alive, its death seen at the look its code brings ([`wake::deaths`](crate::wake::deaths)).
///
/// A message found here is reported, and **every handle it carried is closed**: a late `Ready`
/// carries the server's endpoint, which nothing binds now, and a server whose endpoint has
/// closed ends itself — its exit is then attributed here, as any other's.
pub fn channel_peer_closed(ch: &Endpoint) -> bool {
    let mut msg = Msg::new();
    for _ in 0..PER_PASS {
        // What came closes as `sent` drops, at the end of this pass.
        let sent = match ch.recv(&mut msg) {
            Ok(sent) => sent,
            Err(e) if e.code() == KError::PeerClosed => return true,
            Err(_) => return false, // WouldBlock (alive and quiet), or an error we cannot act on
        };
        let count = sent.len();
        Line::new()
            .s(b"service-mgr: unexpected message on a control channel (dropped, and ")
            .u(count as u64)
            .s(b" handle(s) it carried closed)")
            .end();
    }
    false
}

#[cfg(test)]
mod tests;
