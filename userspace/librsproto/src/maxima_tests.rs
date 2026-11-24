//! **Each named maximum is held at itself and one past**, by its writer and its reader: a reader
//! taking one past would hand its caller more than the caller sized for.

use crate::clipboard::{self, CLIP_RING, ClipInfo, ClipList, MAX_CLIP_BYTES, PathVerb};
use crate::desktop::{self, DesktopEntry, DesktopList, MAX_DESKTOP_NAME};
use crate::surface::{
    DROP_KIND_FILE, DeclareAcceptor, DroppedEvent, MAX_ACCEPTOR_NAME, MAX_DROP_NAME, MAX_DROP_PATH, StartDrag,
};
use std::vec::Vec;

static LETTERS: [u8; 4096] = [b'a'; 4096];

fn letters(n: usize) -> &'static str {
    core::str::from_utf8(&LETTERS[..n]).unwrap()
}

/// `head`'s words, then each of `tails`: a body as a writer that ignored the maximum would send it.
fn body(head: &[u32], tails: &[&str]) -> Vec<u8> {
    let mut b: Vec<u8> = head.iter().flat_map(|w| w.to_le_bytes()).collect();
    tails.iter().for_each(|t| b.extend_from_slice(t.as_bytes()));
    b
}

#[test]
fn a_clipboard_holds_its_bytes_and_its_ring() {
    let mut out = std::vec![0u8; 2 * MAX_CLIP_BYTES];
    let paths = |n: usize, out: &mut [u8]| clipboard::encode_paths(PathVerb::Copy, &[letters(n)], out);
    // "copy", a newline, then the path.
    assert_eq!(paths(MAX_CLIP_BYTES - 5, &mut out), Some(MAX_CLIP_BYTES));
    assert_eq!(paths(MAX_CLIP_BYTES - 4, &mut out), None, "a path list one past");
    let rows = [ClipInfo::default(); CLIP_RING + 1];
    let n = clipboard::write_list(&mut out, 1, &rows[..CLIP_RING]).unwrap();
    assert_eq!(ClipList::read(&out[..n]).map(|l| l.count()), Some(CLIP_RING));
    assert_eq!(clipboard::write_list(&mut out, 1, &rows), None, "a ring of one more");
    let mut more = body(&[1, 0, CLIP_RING as u32 + 1, 0], &[]);
    more.resize(more.len() + (CLIP_RING + 1) * clipboard::CLIP_INFO_LEN, 0);
    assert!(ClipList::read(&more).is_none(), "and read");
}

#[test]
fn a_desktop_list_holds_its_names() {
    let mut out = [0u8; 256];
    let list =
        |n: usize, out: &mut [u8]| desktop::write_list(out, 1, &[DesktopEntry { id: 1, name: letters(n) }], false);
    let n = list(MAX_DESKTOP_NAME, &mut out).unwrap();
    assert_eq!(DesktopList::read(&out[..n]).unwrap().entries().count(), 1);
    assert_eq!(list(MAX_DESKTOP_NAME + 1, &mut out), None, "a name one past");
    let more = body(&[1, 1, 0, 1, MAX_DESKTOP_NAME as u32 + 1], &[letters(MAX_DESKTOP_NAME + 1)]);
    assert_eq!(DesktopList::read(&more).unwrap().entries().count(), 0, "and read");
}

#[cfg(feature = "io")]
#[test]
fn a_desktop_opens_a_path_of_its_maximum_and_refuses_one_past() {
    use crate::RS_FLAG_REPLY;
    use crate::desktop::{Desktop, DesktopError, MAX_OPEN_PATH, OP_DESKTOP_OPEN};
    use crate::session::fixture::{queue, request, service};
    libos::mock::reset();
    let (ns, peer) = service("/dev/desktop");
    let mut msg = libos::Msg::new();
    let mut d = Desktop::resolve(ns, &mut msg).unwrap();
    queue(peer, 1, OP_DESKTOP_OPEN, RS_FLAG_REPLY, b"");
    assert_eq!(d.open(letters(MAX_OPEN_PATH).as_bytes()), Ok(()));
    assert_eq!(request(peer).2, letters(MAX_OPEN_PATH).as_bytes());
    queue(peer, 2, OP_DESKTOP_OPEN, RS_FLAG_REPLY, b"");
    assert_eq!(d.open(letters(MAX_OPEN_PATH + 1).as_bytes()), Err(DesktopError::Protocol), "not sent");
}

#[test]
fn an_acceptor_holds_its_name() {
    let mut out = [0u8; 128];
    let head = DeclareAcceptor { window: 1, kinds: DROP_KIND_FILE };
    let n = head.write(&mut out, letters(MAX_ACCEPTOR_NAME).as_bytes()).unwrap();
    assert!(DeclareAcceptor::read(&out[..n]).is_some());
    assert_eq!(head.write(&mut out, letters(MAX_ACCEPTOR_NAME + 1).as_bytes()), None, "a name one past");
    let more = body(&[1, DROP_KIND_FILE], &[letters(MAX_ACCEPTOR_NAME + 1)]);
    assert!(DeclareAcceptor::read(&more).is_none(), "and read");
}

#[test]
fn a_drag_holds_its_path_and_its_name() {
    let mut out = [0u8; 1024];
    let head = StartDrag { window: 1, kind: DROP_KIND_FILE, path_len: 0 };
    let (path, name) = (letters(MAX_DROP_PATH), letters(MAX_DROP_NAME));
    let n = head.write(&mut out, path.as_bytes(), name.as_bytes()).unwrap();
    assert_eq!(StartDrag::read(&out[..n]).map(|(_, p, m)| (p, m)), Some((path, name)));
    let write = |p: &str, m: &str| head.write(&mut [0u8; 1024], p.as_bytes(), m.as_bytes());
    assert_eq!(write(letters(MAX_DROP_PATH + 1), ""), None, "a path one past");
    assert_eq!(write("/", letters(MAX_DROP_NAME + 1)), None, "a name one past");
    let read = |p: &str, m: &str| StartDrag::read(&body(&[1, DROP_KIND_FILE, p.len() as u32], &[p, m])).is_some();
    assert!(!read(letters(MAX_DROP_PATH + 1), ""), "a path, read");
    assert!(!read("/", letters(MAX_DROP_NAME + 1)), "a name, read");
}

#[test]
fn a_drop_holds_its_three_names() {
    let mut out = [0u8; 1024];
    let head = DroppedEvent { window: 1, kind: DROP_KIND_FILE, ..DroppedEvent::default() };
    let (a, p, m) = (letters(MAX_ACCEPTOR_NAME), letters(MAX_DROP_PATH), letters(MAX_DROP_NAME));
    let n = head.write(&mut out, a.as_bytes(), p.as_bytes(), m.as_bytes()).unwrap();
    assert_eq!(DroppedEvent::read(&out[..n]).map(|(_, a, p, m)| (a, p, m)), Some((a, p, m)));
    let write = |a: &str, p: &str, m: &str| head.write(&mut [0u8; 1024], a.as_bytes(), p.as_bytes(), m.as_bytes());
    assert_eq!(write(letters(MAX_ACCEPTOR_NAME + 1), "/", ""), None, "an acceptor one past");
    assert_eq!(write("a", letters(MAX_DROP_PATH + 1), ""), None, "a path one past");
    assert_eq!(write("a", "/", letters(MAX_DROP_NAME + 1)), None, "a name one past");
    let read = |a: &str, p: &str, m: &str| {
        let head = [1, DROP_KIND_FILE, 0, 0, a.len() as u32, p.len() as u32];
        DroppedEvent::read(&body(&head, &[a, p, m])).is_some()
    };
    assert!(!read(letters(MAX_ACCEPTOR_NAME + 1), "/", ""), "an acceptor, read");
    assert!(!read("a", letters(MAX_DROP_PATH + 1), ""), "a path, read");
    assert!(!read("a", "/", letters(MAX_DROP_NAME + 1)), "a name, read");
}
