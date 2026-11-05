//! Authenticating against auth-service, and a session's registration with the view broker.

use core::sync::atomic::{AtomicU64, Ordering};
use libkern::{Secret, scrub};
use libos::{Channel, Msg, Only, Ref};
use librsproto::auth::{build_authenticate_request, parse_authenticate_reply};
use librsproto::{OP_AUTHENTICATE, decode};

/// A message scrubbed where it lies when it goes out of scope, on every way out: a password passed
/// through it, and a move of the message would leave a copy.
struct Scrubbed(Msg);

impl Drop for Scrubbed {
    fn drop(&mut self) {
        scrub(self.0.payload_mut());
    }
}

/// Authenticate `(user, pass)` against auth-service over `auth_ch`, copying the home path into
/// `home_out`. `Some(home_len)` if AUTHENTICATED, `None` if DENIED or on any protocol error.
pub fn authenticate_on<'a>(
    auth_ch: impl Into<Ref<'a, Channel, Only>>,
    user: &[u8],
    pass: &[u8],
    home_out: &mut [u8],
) -> Option<usize> {
    // **Both copies of the password go on every return**: a supervisor's stack outlives the call.
    let mut body = Secret::<512>::zeroed();
    let n = build_authenticate_request(&mut body, user, pass)?;
    let mut msg = Scrubbed(Msg::new());
    let reply = call(auth_ch.into(), OP_AUTHENTICATE, &body[..n], &mut msg)?;
    home_of(reply, home_out)
}

/// The home an `Authenticate` reply names, copied into `home_out`; `None` unless AUTHENTICATED with
/// a home that fits, which is never cut short.
pub(crate) fn home_of(reply: &[u8], home_out: &mut [u8]) -> Option<usize> {
    let m = decode(reply).ok()?;
    let r = parse_authenticate_reply(m.body).filter(|r| r.is_authenticated())?;
    home_out.get_mut(..r.home.len())?.copy_from_slice(r.home);
    Some(r.home.len())
}

/// The request id of the next [`call`], so one left unanswered never answers a later one.
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// One request on `ch`, its own id, through `msg`: the reply's rsproto payload. A [`Scrubbed`]
/// message, so a request that carries a password cannot be sent through one that is not.
fn call<'m>(ch: Ref<'_, Channel, Only>, op: u16, body: &[u8], msg: &'m mut Scrubbed) -> Option<&'m [u8]> {
    let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    let n = librsproto::session::exchange(ch, &mut msg.0, id, op, body).ok()?;
    Some(&msg.0.payload()[..n])
}

/// One request to the view broker on `ch`: `None` if it failed, was refused or will not fit `out`.
fn views_call(ch: Ref<'_, Channel, Only>, op: u16, body: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut msg = Scrubbed(Msg::new());
    let m = decode(call(ch, op, body, &mut msg)?).ok().filter(|m| !m.is_error())?;
    out.get_mut(..m.body.len())?.copy_from_slice(m.body);
    Some(m.body.len())
}

/// Open a session with the view broker for `principal` over `sup`, writing its base `/s/<id>` into
/// `base_out`. Returns the id and the base's length, or `None` if the broker did not answer.
pub fn open_views_session<'a>(
    sup: impl Into<Ref<'a, Channel, Only>>,
    principal: &[u8],
    base_out: &mut [u8; 24],
) -> Option<(u64, usize)> {
    let mut reply = [0u8; 8];
    let n = views_call(sup.into(), librsproto::views::OP_VIEWS_OPEN_SESSION, principal, &mut reply)?;
    let id = librsproto::views::parse_session_id(&reply[..n])?;
    // `/s/` and at most twenty digits: 23 bytes, inside the 24 the caller gives.
    let mut buf = [0u8; 20];
    let d = libkern::debug::fmt_u64(id, &mut buf);
    base_out[..3].copy_from_slice(b"/s/");
    base_out[3..3 + d.len()].copy_from_slice(d);
    Some((id, 3 + d.len()))
}

/// Tell the view broker that session `id` has ended. **The supervisor's job**: a program in a view
/// still binds `/dev/views`, so the registration outlives the login.
pub fn close_views_session<'a>(sup: impl Into<Ref<'a, Channel, Only>>, id: u64) {
    let mut body = [0u8; 8];
    if let Some(n) = librsproto::views::build_session_id(&mut body, id) {
        let _ = views_call(sup.into(), librsproto::views::OP_VIEWS_CLOSE_SESSION, &body[..n], &mut [0u8; 1]);
    }
}

#[cfg(test)]
mod tests;
