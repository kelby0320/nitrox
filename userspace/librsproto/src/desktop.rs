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
    fn an_index_round_trips_and_refuses_a_short_body() {
        let mut b = [0u8; 4];
        DesktopIndex { index: 3 }.write(&mut b).unwrap();
        assert_eq!(DesktopIndex::read(&b).unwrap().index, 3);
        assert_eq!(DesktopIndex::read(&b[..3]), None);
    }
}
