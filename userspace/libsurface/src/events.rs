//! What a server event means to one window, decoded apart from the session that queues it.

use crate::WindowEvent;
use alloc::string::String;
use librsproto::surface::{
    DroppedEvent, FocusEvent, KeyEvent, OP_CLOSE_REQUESTED, OP_DISMISSED, OP_DROPPED, OP_FOCUS_EVENT, OP_KEY_EVENT,
    OP_POINTER_EVENT, OP_WINDOW_STATE, PointerEvent, WindowRef, WindowState,
};

/// The window a server event is for and what it means there, for an event that only queues; `None`
/// for a body that does not decode, or an op that changes the session too (a release, a configure).
pub(crate) fn window_event(op: u16, body: &[u8]) -> Option<(u32, WindowEvent)> {
    Some(match op {
        OP_KEY_EVENT => {
            let e = KeyEvent::read(body)?;
            (e.window, WindowEvent::Key(e))
        }
        OP_POINTER_EVENT => {
            let e = PointerEvent::read(body)?;
            (e.window, WindowEvent::Pointer(e))
        }
        OP_FOCUS_EVENT => {
            let e = FocusEvent::read(body)?;
            (e.window, WindowEvent::Focus(e.focused != 0))
        }
        OP_DROPPED => {
            let (e, acceptor, path, name) = DroppedEvent::read(body)?;
            let (acceptor, path, name) = (String::from(acceptor), String::from(path), String::from(name));
            (e.window, WindowEvent::Drop { acceptor, kind: e.kind, path, name, x: e.x, y: e.y })
        }
        OP_CLOSE_REQUESTED => (WindowRef::read(body)?.window, WindowEvent::CloseRequested),
        OP_DISMISSED => (WindowRef::read(body)?.window, WindowEvent::Dismissed),
        OP_WINDOW_STATE => {
            let s = WindowState::read(body)?;
            (s.window, WindowEvent::State(s.state))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::surface::{OP_CONFIGURE, OP_RELEASE};

    #[test]
    fn a_window_state_is_its_window_and_the_state_it_names() {
        // By hand: the manager's word for window 9 is maximised.
        let body = [9, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!(window_event(OP_WINDOW_STATE, &body), Some((9, WindowEvent::State(2))));
        assert_eq!(window_event(OP_WINDOW_STATE, &body[..7]), None);
    }

    #[test]
    fn each_queued_event_names_its_window() {
        let window_ref = [5, 0, 0, 0];
        assert_eq!(window_event(OP_CLOSE_REQUESTED, &window_ref), Some((5, WindowEvent::CloseRequested)));
        assert_eq!(window_event(OP_DISMISSED, &window_ref), Some((5, WindowEvent::Dismissed)));
        let mut b = [0u8; 16];
        let f = FocusEvent { focused: 1, _pad: 0, window: 6 };
        let n = f.write(&mut b).expect("fits");
        assert_eq!(window_event(OP_FOCUS_EVENT, &b[..n]), Some((6, WindowEvent::Focus(true))));
        let k = KeyEvent::new(7, 30, 1, 0);
        let n = k.write(&mut b).expect("fits");
        assert_eq!(window_event(OP_KEY_EVENT, &b[..n]), Some((7, WindowEvent::Key(k))));
        let mut pb = [0u8; 64];
        let p = PointerEvent { window: 8, x: 3, y: 4, ..Default::default() };
        let n = p.write(&mut pb).expect("fits");
        assert_eq!(window_event(OP_POINTER_EVENT, &pb[..n]), Some((8, WindowEvent::Pointer(p))));
    }

    #[test]
    fn a_drop_carries_its_names_and_an_op_that_changes_the_session_is_not_one_of_these() {
        let mut b = [0u8; 128];
        let d = DroppedEvent { window: 4, kind: 1, x: 10, y: 20, ..Default::default() };
        let n = d.write(&mut b, b"files", b"/home/a.txt", b"a.txt").expect("fits");
        let (window, event) = window_event(OP_DROPPED, &b[..n]).expect("decodes");
        assert_eq!(window, 4);
        let WindowEvent::Drop { acceptor, path, name, x, y, .. } = event else { panic!("not a drop") };
        assert_eq!((acceptor.as_str(), path.as_str(), name.as_str(), x, y), ("files", "/home/a.txt", "a.txt", 10, 20));
        assert_eq!(window_event(OP_CONFIGURE, &b[..n]), None);
        assert_eq!(window_event(OP_RELEASE, &b[..n]), None);
    }
}
