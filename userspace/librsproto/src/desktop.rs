//! `Desktop` — the shell's own resource, served at `/dev/desktop`.
//!
//! **A session channel, not a path per object.** The bare resolve is answered with a session
//! the way `/dev/draw/new` and `/dev/tty` are, because the operations that matter here are
//! *mutations* (switch, name) and a namespace resolve is a lookup rather than a call; per-object
//! paths would duplicate what `List` returns. See `ui-composition-model.md` §2a.
//!
//! **The desktop shell serves this**, so it is the one process that both serves a resource and
//! constructs namespaces (`graphical-session.md` §3).

use crate::{Reader, put_u32};

/// Longest desktop name on the wire, in bytes.
///
/// Bounded like every other variable-length field here; a full list must fit one message.
pub const MAX_DESKTOP_NAME: usize = 32;

/// How many desktops a `List` reply may describe.
///
/// Truncation is reported, not implied: see [`DesktopList::truncated`].
pub const MAX_LISTED: usize = 16;

/// `Desktop::List` — describe every desktop, and which one is current.
pub const OP_DESKTOP_LIST: u16 = 0x0C00;
/// `Desktop::Switch` — make the *n*th desktop current. Body: [`DesktopIndex`].
pub const OP_DESKTOP_SWITCH: u16 = 0x0C01;
/// `Desktop::Name` — name the *n*th desktop. Body: [`DesktopIndex`] then UTF-8 bytes.
///
/// **Naming is what makes a desktop persist.**
pub const OP_DESKTOP_NAME: u16 = 0x0C02;

/// `Desktop::Open` — open a path with whatever program the shell says opens it. Body: the
/// path's UTF-8 bytes, at most [`MAX_OPEN_PATH`].
///
/// **The client names the path; the shell decides what runs.** `InvalidArgument` is the path's
/// *shape* (empty, over the cap, not UTF-8, or not absolute); `Unsupported` is **the launch
/// failing**, not the file. There is no `NotFound`: the shell does not resolve the path
/// (`rsproto-desktop-ops.md` § Open; see [`DesktopError`]).
pub const OP_DESKTOP_OPEN: u16 = 0x0C03;

/// Longest path an [`Open`](OP_DESKTOP_OPEN) may name, in bytes.
pub const MAX_OPEN_PATH: usize = 512;

/// Which desktop an op names — a **position**, one-based, as a person counts them.
///
/// One-based, as a person counts them; not an id (`rsproto-desktop-ops.md` § Positions).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DesktopIndex {
    /// One-based position in the list.
    pub index: u32,
}

impl DesktopIndex {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 4 {
            return None;
        }
        put_u32(out, 0, self.index);
        Some(4)
    }

    /// Parse from the first 4 bytes of a body.
    pub fn read(b: &[u8]) -> Option<Self> {
        Some(Self { index: Reader::new(b).u32()? })
    }
}

/// One entry in a [`List`](OP_DESKTOP_LIST) reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DesktopEntry<'a> {
    /// Stable id, for a caller that wants to hold on to one across a renumbering.
    pub id: u32,
    /// The name, or empty when it has none — which is also what says it will not persist.
    pub name: &'a str,
}

/// Write a `List` reply: `count`, `current` (one-based, 0 if none), then each entry.
///
/// Each entry is `id` (u32), `len` (u32), then `len` UTF-8 bytes. Returns the length written,
/// or `None` if `out` is too short — **refused rather than truncated**, because a half-written
/// list parses as a shorter one and a caller cannot tell.
pub fn write_list(out: &mut [u8], current: u32, entries: &[DesktopEntry<'_>], truncated: bool) -> Option<usize> {
    let mut n = 12;
    if out.len() < n {
        return None;
    }
    put_u32(out, 0, entries.len() as u32);
    put_u32(out, 4, current);
    put_u32(out, 8, u32::from(truncated));
    for e in entries {
        let bytes = e.name.as_bytes();
        if bytes.len() > MAX_DESKTOP_NAME || out.len() < n + 8 + bytes.len() {
            return None;
        }
        put_u32(out, n, e.id);
        put_u32(out, n + 4, bytes.len() as u32);
        out[n + 8..n + 8 + bytes.len()].copy_from_slice(bytes);
        n += 8 + bytes.len();
    }
    Some(n)
}

/// A parsed `List` reply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DesktopList<'a> {
    /// The raw entry bytes, walked by [`entries`](Self::entries).
    body: &'a [u8],
    /// How many entries the body holds.
    pub count: u32,
    /// The current desktop's one-based position, or `0`.
    pub current: u32,
    /// Whether the server had more desktops than it could describe.
    pub truncated: bool,
}

impl<'a> DesktopList<'a> {
    /// Parse a `List` reply.
    pub fn read(b: &'a [u8]) -> Option<Self> {
        let mut r = Reader::new(b);
        let (count, current, truncated) = (r.u32()?, r.u32()?, r.u32()? != 0);
        Some(Self { body: r.rest(), count, current, truncated })
    }

    /// Walk the entries. Stops early on a malformed one rather than guessing at its length.
    pub fn entries(&self) -> impl Iterator<Item = DesktopEntry<'a>> {
        let mut r = Reader::new(self.body);
        let mut left = self.count;
        core::iter::from_fn(move || {
            if left == 0 {
                return None;
            }
            let entry = (|| {
                let id = r.u32()?;
                let len = r.u32()? as usize;
                if len > MAX_DESKTOP_NAME {
                    return None;
                }
                Some(DesktopEntry { id, name: core::str::from_utf8(r.bytes(len)?).ok()? })
            })();
            left = if entry.is_some() { left - 1 } else { 0 };
            entry
        })
    }
}

/// A client of `/dev/desktop`: an endpoint and the one message its traffic uses.
///
/// The message is the caller's, so this crate stays `alloc`-free. A client built by
/// [`resolve`](Self::resolve) or [`new`](Self::new) owns its endpoint and closes it when dropped;
/// one built by the raw [`connect`](Self::connect) or [`from_endpoint`](Self::from_endpoint) is
/// closed only by [`close`](Self::close).
#[cfg(feature = "io")]
pub struct Desktop<'a> {
    session: crate::session::Session<'a>,
}

/// What a `/dev/desktop` request can fail with.
///
/// Three cases, so a dead channel never reads as a bad operand.
#[cfg(feature = "io")]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DesktopError {
    /// A syscall failed or the reply did not decode; the payload is its negative return, if any.
    Transport(i32),
    /// The request did not fit the message buffer.
    Protocol,
    /// The shell replied with an error: its `KError`, or `None` if the reply carried no body.
    Refused(Option<i32>),
}

#[cfg(feature = "io")]
impl From<crate::session::WireError> for DesktopError {
    fn from(e: crate::session::WireError) -> Self {
        match e {
            crate::session::WireError::Transport(c) => DesktopError::Transport(c),
            crate::session::WireError::Protocol => DesktopError::Protocol,
        }
    }
}

#[cfg(feature = "io")]
impl<'a> Desktop<'a> {
    /// Resolve `/dev/desktop` in `ns` and wrap the session it answers with, which the client
    /// owns and closes.
    pub fn resolve<'n, M: libos::CanLookup + 'n>(
        ns: impl Into<libos::Ref<'n, libos::Namespace, M>>,
        msg: &'a mut libos::Msg,
    ) -> Result<Desktop<'a>, DesktopError> {
        Ok(Self::new(crate::session::connect(ns, "/dev/desktop").map_err(DesktopError::from)?, msg))
    }

    /// Wrap an endpoint resolved elsewhere, which the client owns and closes.
    pub fn new(endpoint: libos::Handle<libos::Channel, libos::Only>, msg: &'a mut libos::Msg) -> Desktop<'a> {
        Desktop { session: crate::session::Session::new(endpoint, msg) }
    }

    /// Resolve `/dev/desktop` in `ns` and wrap the session it answers with.
    ///
    /// `buf` must be at least [`IPC_MSG_SIZE`](libkern::abi::IPC_MSG_SIZE) bytes. For callers not
    /// yet on libos; the typed form is [`resolve`](Self::resolve).
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub fn connect(ns: u64, buf: &'a mut [u8]) -> Result<Desktop<'a>, DesktopError> {
        if buf.len() < libkern::abi::IPC_MSG_SIZE {
            return Err(DesktopError::Protocol);
        }
        // SAFETY: the caller's own namespace handle, open for the call, as this has always asked.
        let ns = unsafe {
            libos::Ref::<libos::Namespace, libos::NsReadOnly>::from_raw(
                libkern::RawHandle(ns),
                libkern::Rights::empty(),
            )
        };
        let endpoint = crate::session::connect(ns, "/dev/desktop").map_err(DesktopError::from)?;
        Ok(Desktop { session: crate::session::Session::raw(endpoint.into_raw().0, buf) })
    }

    /// Wrap an endpoint resolved elsewhere. Takes ownership: [`close`](Self::close) closes it. For
    /// callers not yet on libos; the typed form is [`new`](Self::new).
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub fn from_endpoint(endpoint: u64, buf: &'a mut [u8]) -> Result<Desktop<'a>, DesktopError> {
        if buf.len() < libkern::abi::IPC_MSG_SIZE {
            return Err(DesktopError::Protocol);
        }
        Ok(Desktop { session: crate::session::Session::raw(endpoint, buf) })
    }

    /// Close the session's endpoint, whichever way the client holds it.
    pub fn close(self) {
        self.session.close();
    }

    /// Send one request and copy its reply body into `out`, returning the length written.
    pub fn request(&mut self, op: u16, body: &[u8], out: &mut [u8]) -> Result<usize, DesktopError> {
        let payload = self.session.call(op, body).map_err(DesktopError::from)?;
        let msg = crate::decode(payload).map_err(|_| DesktopError::Transport(0))?;
        if msg.flags & crate::RS_FLAG_ERROR != 0 {
            // The shell's error reply is its `KError` as four little-endian bytes.
            let code =
                (msg.body.len() >= 4).then(|| i32::from_le_bytes([msg.body[0], msg.body[1], msg.body[2], msg.body[3]]));
            return Err(DesktopError::Refused(code));
        }
        let n = msg.body.len().min(out.len());
        out[..n].copy_from_slice(&msg.body[..n]);
        Ok(n)
    }

    /// Ask the shell to open `path` with whatever program opens it — [`Open`](OP_DESKTOP_OPEN).
    ///
    /// Returns once the shell has decided, before the program has drawn anything.
    pub fn open(&mut self, path: &[u8]) -> Result<(), DesktopError> {
        if path.is_empty() || path.len() > MAX_OPEN_PATH {
            return Err(DesktopError::Protocol);
        }
        let mut out = [0u8; 8];
        self.request(OP_DESKTOP_OPEN, path, &mut out).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_round_trips_with_names_and_without() {
        let mut buf = [0u8; 256];
        let entries = [DesktopEntry { id: 4, name: "work" }, DesktopEntry { id: 7, name: "" }];
        let n = write_list(&mut buf, 1, &entries, false).unwrap();
        let list = DesktopList::read(&buf[..n]).unwrap();
        assert_eq!(list.count, 2);
        assert_eq!(list.current, 1);
        assert!(!list.truncated);
        let mut it = list.entries();
        assert_eq!(it.next(), Some(entries[0]));
        assert_eq!(it.next(), Some(entries[1]), "an unnamed desktop round-trips as an empty name");
        assert_eq!(it.next(), None);
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_written_partially() {
        let entries = [DesktopEntry { id: 1, name: "alpha" }];
        let mut small = [0u8; 16];
        assert_eq!(write_list(&mut small, 1, &entries, false), None);
        let mut exact = [0u8; 12 + 8 + 5];
        assert!(write_list(&mut exact, 1, &entries, false).is_some());
    }

    #[test]
    fn truncation_is_reported_rather_than_implied() {
        let mut buf = [0u8; 64];
        let n = write_list(&mut buf, 1, &[DesktopEntry { id: 1, name: "" }], true).unwrap();
        assert!(DesktopList::read(&buf[..n]).unwrap().truncated);
    }

    #[test]
    fn a_body_claiming_more_entries_than_it_holds_stops_rather_than_inventing() {
        let mut buf = [0u8; 64];
        let n = write_list(&mut buf, 1, &[DesktopEntry { id: 1, name: "x" }], false).unwrap();
        put_u32(&mut buf, 0, 2);
        let list = DesktopList::read(&buf[..n]).unwrap();
        assert_eq!(list.entries().count(), 1, "the walk invented a second entry");
    }

    #[test]
    fn a_name_length_past_the_end_of_the_body_is_refused() {
        // The `8 + len` half of the walk's guard must not panic on the slice.
        // count=1, current=1, truncated=0; entry id=1, len=9, then two bytes only.
        let mut b = [0u8; 22];
        for (i, v) in [1u32, 1, 0, 1, 9].iter().enumerate() {
            put_u32(&mut b, i * 4, *v);
        }
        b[20] = b'a';
        b[21] = b'b';
        let list = DesktopList::read(&b).unwrap();
        assert_eq!(list.entries().count(), 0, "a name running past the body was accepted");
    }

    #[test]
    fn an_index_round_trips_and_refuses_a_short_body() {
        let mut b = [0u8; 4];
        DesktopIndex { index: 3 }.write(&mut b).unwrap();
        assert_eq!(DesktopIndex::read(&b).unwrap().index, 3);
        assert_eq!(DesktopIndex::read(&b[..3]), None);
    }

    #[test]
    fn parsers_survive_truncation_and_corruption() {
        use crate::fuzz::*;
        let mut b = [0u8; 64];
        let n = DesktopIndex { index: 3 }.write(&mut b).unwrap();
        refuses_every_prefix(&b[..n], |x| DesktopIndex::read(x).is_some());
        let n = write_list(&mut b, 1, &[DesktopEntry { id: 1, name: "one" }, DesktopEntry { id: 2, name: "" }], false)
            .unwrap();
        for k in 0..12 {
            assert!(DesktopList::read(&b[..k]).is_none());
        }
        let walk = |x: &[u8]| DesktopList::read(x).map(|l| l.entries().count());
        survives_corruption(&b[..n], |x| walk(x).is_some());
        assert_eq!(walk(&b[..n]), Some(2));
        assert_eq!(walk(&b[..n - 1]), Some(1), "a cut-off second header is dropped");
    }

    #[cfg(feature = "io")]
    mod io {
        use super::*;
        use crate::session::fixture::{queue, request, service};
        use libos::{Msg, mock};

        #[test]
        fn an_open_is_sent_to_the_shell_and_its_endpoint_goes_with_the_client() {
            mock::reset();
            let (ns, peer) = service("/dev/desktop");
            let mut msg = Msg::new();
            let mut desktop = Desktop::resolve(ns, &mut msg).unwrap();
            let open = mock::open_handles();
            queue(peer, 1, OP_DESKTOP_OPEN, crate::RS_FLAG_REPLY, b"");
            assert_eq!(desktop.open(b"/home/a.txt"), Ok(()));
            let (id, op, body) = request(peer);
            assert_eq!((id, op, body.as_slice()), (1, OP_DESKTOP_OPEN, &b"/home/a.txt"[..]));
            let refusal = libkern::KError::Unsupported.as_i32().to_le_bytes();
            queue(peer, 2, OP_DESKTOP_OPEN, crate::RS_FLAG_REPLY | crate::RS_FLAG_ERROR, &refusal);
            assert_eq!(desktop.open(b"/x"), Err(DesktopError::Refused(Some(libkern::KError::Unsupported.as_i32()))));
            drop(desktop);
            assert_eq!(mock::open_handles(), open - 1);
        }

        #[test]
        fn a_missing_desktop_is_a_transport_fault_and_a_raw_client_closes_only_by_close() {
            mock::reset();
            let (ns, _peer) = service("/dev/desktop");
            let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
            assert!(matches!(Desktop::connect(0, &mut buf[..16]), Err(DesktopError::Protocol)));
            let (other, _p) = service("/dev/other");
            assert_eq!(
                Desktop::resolve(other, &mut Msg::new()).err(),
                Some(DesktopError::Transport(libkern::KError::NotFound.as_i32()))
            );
            let desktop = Desktop::connect(ns.raw().0, &mut buf).unwrap();
            let open = mock::open_handles();
            drop(desktop);
            assert_eq!(mock::open_handles(), open, "a raw client is closed by `close` alone");
            Desktop::connect(ns.raw().0, &mut buf).unwrap().close();
            assert_eq!(mock::open_handles(), open);
            assert_eq!(mock::stray_closes(), 0);
        }
    }
}
