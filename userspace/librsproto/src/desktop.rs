//! `Desktop` — the shell's own resource, served at `/dev/desktop`.
//!
//! **A session channel, not a path per object.** `ui-composition-model.md` §2a sketches
//! `new`, `current`, `N/info` and `N/windows/` as separate paths. What is built is the bare
//! resolve, answered with a session the way `/dev/draw/new` and `/dev/tty` are — because the
//! operations that matter here are *mutations* (switch, name), and a namespace resolve is a
//! lookup rather than a call. The per-object paths would duplicate what `List` already returns,
//! for no consumer, which is the shape this project refuses.
//!
//! **The desktop shell serves this**, which makes it the one process that both serves a
//! resource and constructs namespaces. `graphical-session.md` §3 is where that is reconciled.

use crate::{get_u32, put_u32};

/// Longest desktop name on the wire, in bytes.
///
/// Bounded like every other variable-length field here. Long enough for a label a person types
/// and short enough that a full list fits one message.
pub const MAX_DESKTOP_NAME: usize = 32;

/// How many desktops a `List` reply may describe.
///
/// The shell's own list is unbounded — desktops are created on demand — so a reply that could
/// not say "there are more" would be lying by omission. It says so instead: see
/// [`DesktopList::truncated`].
pub const MAX_LISTED: usize = 16;

/// `Desktop::List` — describe every desktop, and which one is current.
pub const OP_DESKTOP_LIST: u16 = 0x0C00;
/// `Desktop::Switch` — make the *n*th desktop current. Body: [`DesktopIndex`].
pub const OP_DESKTOP_SWITCH: u16 = 0x0C01;
/// `Desktop::Name` — name the *n*th desktop. Body: [`DesktopIndex`] then UTF-8 bytes.
///
/// **Naming is what makes a desktop persist** (`display-arm-plan.md` M8 Part D), so this is the
/// one op here that changes which desktops survive rather than which is showing.
pub const OP_DESKTOP_NAME: u16 = 0x0C02;

/// `Desktop::Open` — open a path with whatever program the shell says opens it. Body: the
/// path's UTF-8 bytes, at most [`MAX_OPEN_PATH`].
///
/// **The client names the path, never the program**, and that is the whole of the design. An
/// application holds no authority to spawn anything: `desktop-shell` is the process with the
/// `/bin` to resolve an image from and the ability to build the namespace a new application
/// runs in (`graphical-session.md` §3). A request naming a *program* would be asking the shell
/// to run arbitrary code on the caller's say-so, which is ambient authority wearing a protocol;
/// a request naming a *path* asks a question the shell already answers for its own launcher.
///
/// **What the reply means.** Success says the shell launched something, not that the program
/// could read the file — an editor opened on a path it cannot read reports that in its own
/// window, where the person who asked for it is looking. `NotFound` is the shell declining
/// because nothing there resolves; `Unsupported` because nothing is registered to open it.
///
/// M10 Part D, and the first op here a *client* sends about something other than desktops. That
/// it lives on this resource rather than a new one is deliberate: `/dev/desktop` is already the
/// shell's channel, already bound into every application namespace, and a second endpoint for
/// one op would be a binding to audit for no gain.
pub const OP_DESKTOP_OPEN: u16 = 0x0C03;

/// Longest path an [`Open`](OP_DESKTOP_OPEN) may name, in bytes.
///
/// Bounded like every other variable-length field here, and generously: a path is composed by
/// walking a tree, so the bound wants to be above what a person can reach rather than above
/// what a form can hold.
pub const MAX_OPEN_PATH: usize = 512;

/// Which desktop an op names — a **position**, one-based, as a person counts them.
///
/// Not an id: ids are stable and never reused, so after a few desktops have come and gone they
/// stop matching what the indicator shows, and `Super+N` addresses positions for the same
/// reason.
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
        if b.len() < 4 {
            return None;
        }
        Some(Self { index: get_u32(b, 0) })
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
        if b.len() < 12 {
            return None;
        }
        Some(Self {
            body: &b[12..],
            count: get_u32(b, 0),
            current: get_u32(b, 4),
            truncated: get_u32(b, 8) != 0,
        })
    }

    /// Walk the entries. Stops early on a malformed one rather than guessing at its length.
    pub fn entries(&self) -> impl Iterator<Item = DesktopEntry<'a>> {
        let mut rest = self.body;
        let mut left = self.count;
        core::iter::from_fn(move || {
            if left == 0 || rest.len() < 8 {
                return None;
            }
            let id = get_u32(rest, 0);
            let len = get_u32(rest, 4) as usize;
            if rest.len() < 8 + len || len > MAX_DESKTOP_NAME {
                return None;
            }
            let name = core::str::from_utf8(&rest[8..8 + len]).ok()?;
            rest = &rest[8 + len..];
            left -= 1;
            Some(DesktopEntry { id, name })
        })
    }
}

/// A client of `/dev/desktop`: an endpoint handle and the one message buffer its traffic uses.
///
/// **The buffer is the caller's**, exactly as [`Dir`](crate::session::Dir)'s is, so this crate
/// stays `alloc`-free and a coreutil can put 4 KiB on its stack while a graphical client keeps
/// one for its whole run.
///
/// It exists because there are two clients now. `desktop` — the coreutil — hand-rolled the
/// send, the `sys_wait`, the recv and the reply decode, and M10 Part D added `nxfiles` asking
/// the shell to open a file. A second copy of that is how two implementations of one wire
/// format come to disagree, so it moved down here beside the ops it speaks.
#[cfg(feature = "io")]
pub struct Desktop<'a> {
    endpoint: u64,
    buf: &'a mut [u8],
    next_request_id: u64,
}

/// What a `/dev/desktop` request can fail with.
///
/// **Three cases and not one**, which the coreutil learned the expensive way: collapsing them
/// meant a dead channel and a shell answering `Unsupported` both printed *no such desktop* — a
/// diagnosis of the operand for a fault that had nothing to do with it (PR #245 review,
/// finding 9).
#[cfg(feature = "io")]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DesktopError {
    /// A syscall failed, or the reply did not decode: the channel is gone, or the peer is not
    /// speaking this protocol. The payload is the syscall's negative return where there was one.
    Transport(i32),
    /// The request did not fit the message buffer.
    Protocol,
    /// The shell replied with an error. The payload is its `KError`, or `None` if the error
    /// reply carried no body.
    Refused(Option<i32>),
}

#[cfg(feature = "io")]
impl<'a> Desktop<'a> {
    /// Resolve `/dev/desktop` in `ns` and wrap the session it answers with.
    ///
    /// `buf` must be at least [`IPC_MSG_SIZE`](libkern::abi::IPC_MSG_SIZE) bytes.
    pub fn connect(ns: u64, buf: &'a mut [u8]) -> Result<Desktop<'a>, DesktopError> {
        use libkern::abi::IPC_MSG_SIZE;
        use libkern::syscall::{SYS_NS_LOOKUP, syscall4};
        if buf.len() < IPC_MSG_SIZE {
            return Err(DesktopError::Protocol);
        }
        let path = b"/dev/desktop";
        // SAFETY: a valid path slice and a namespace handle this process holds.
        let po = unsafe {
            syscall4(
                SYS_NS_LOOKUP,
                ns,
                path.as_ptr() as u64,
                path.len() as u64,
                crate::session::DIR_SESSION_RIGHTS,
            )
        };
        if po < 0 {
            return Err(DesktopError::Transport(po as i32));
        }
        let (status, endpoint) = crate::session::po_wait(po as u64);
        if status < 0 {
            return Err(DesktopError::Transport(status));
        }
        if endpoint == 0 {
            return Err(DesktopError::Protocol);
        }
        Ok(Desktop { endpoint, buf, next_request_id: 1 })
    }

    /// Wrap an endpoint resolved elsewhere. Takes ownership: [`close`](Self::close) closes it.
    pub fn from_endpoint(endpoint: u64, buf: &'a mut [u8]) -> Result<Desktop<'a>, DesktopError> {
        if buf.len() < libkern::abi::IPC_MSG_SIZE {
            return Err(DesktopError::Protocol);
        }
        Ok(Desktop { endpoint, buf, next_request_id: 1 })
    }

    /// Close the session's endpoint.
    ///
    /// Explicit rather than a `Drop`, for [`Dir`](crate::session::Dir)'s reason: dropping
    /// cannot report a failure, and a handle close is worth not doing silently.
    pub fn close(self) {
        // SAFETY: closing an endpoint this session owns.
        unsafe { libkern::syscall::syscall1(libkern::syscall::SYS_HANDLE_CLOSE, self.endpoint) };
    }

    /// Send one request and copy its reply body into `out`, returning the length written.
    pub fn request(&mut self, op: u16, body: &[u8], out: &mut [u8]) -> Result<usize, DesktopError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let len = crate::session::round_trip(self.endpoint, self.buf, request_id, op, body)
            .map_err(|e| match e {
                crate::session::WireError::Transport(c) => DesktopError::Transport(c),
                crate::session::WireError::Protocol => DesktopError::Protocol,
            })?;
        let payload = &self.buf[24..24 + len];
        let msg = crate::decode(payload).map_err(|_| DesktopError::Transport(0))?;
        if msg.flags & crate::RS_FLAG_ERROR != 0 {
            // The shell's error reply is its `KError` as four little-endian bytes.
            let code = (msg.body.len() >= 4)
                .then(|| i32::from_le_bytes([msg.body[0], msg.body[1], msg.body[2], msg.body[3]]));
            return Err(DesktopError::Refused(code));
        }
        let n = msg.body.len().min(out.len());
        out[..n].copy_from_slice(&msg.body[..n]);
        Ok(n)
    }

    /// Ask the shell to open `path` with whatever program opens it — [`Open`](OP_DESKTOP_OPEN).
    ///
    /// Returns once the shell has answered, which is *before* the program it launched has drawn
    /// anything: what is being waited for is the decision, not the window.
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
        let entries =
            [DesktopEntry { id: 4, name: "work" }, DesktopEntry { id: 7, name: "" }];
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
        // A half-written list parses as a *shorter* list, and a caller cannot tell the
        // difference — so it would silently show fewer desktops than exist.
        let entries = [DesktopEntry { id: 1, name: "alpha" }];
        let mut small = [0u8; 16];
        assert_eq!(write_list(&mut small, 1, &entries, false), None);
        let mut exact = [0u8; 12 + 8 + 5];
        assert!(write_list(&mut exact, 1, &entries, false).is_some());
    }

    #[test]
    fn truncation_is_reported_rather_than_implied() {
        // The shell's desktop list is unbounded, so a reply that could not say "there are
        // more" would be lying by omission — a caller would show a complete-looking list.
        let mut buf = [0u8; 64];
        let n = write_list(&mut buf, 1, &[DesktopEntry { id: 1, name: "" }], true).unwrap();
        assert!(DesktopList::read(&buf[..n]).unwrap().truncated);
    }

    #[test]
    fn a_body_claiming_more_entries_than_it_holds_stops_rather_than_inventing() {
        let mut buf = [0u8; 64];
        let n = write_list(&mut buf, 1, &[DesktopEntry { id: 1, name: "x" }], false).unwrap();
        // Claim two, deliver one.
        put_u32(&mut buf, 0, 2);
        let list = DesktopList::read(&buf[..n]).unwrap();
        assert_eq!(list.entries().count(), 1, "the walk invented a second entry");
    }

    #[test]
    fn a_name_length_past_the_end_of_the_body_is_refused() {
        // **The `8 + len` half of the walk's guard, which nothing reached.** The entry-count
        // test above stops on `rest.len() < 8` — after one entry `rest` is empty — so removing
        // the length check left every test green while a body declaring a longer name than it
        // carries panicked on the slice (PR #245 review, finding 7).
        //
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
}
