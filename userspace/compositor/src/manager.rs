//! The manager channel's request handling: pure, host-tested, no syscalls.
//!
//! The counterpart of [`server::dispatch`](crate::server::dispatch) for the *other* channel role.
//! A client holds a session and speaks about windows it created; a manager holds one channel for
//! the whole compositor and speaks about any window at all.
//!
//! ## The capability is the binding, not a check
//!
//! **Nothing here verifies ownership, and that is the point.** A manager manages windows it did
//! not create — that is what makes it a manager — so the boundary is *who may hold this channel*,
//! not *what may be asked over it*. `/dev/draw/manage` is where that boundary lives.
//!
//! **In Milestone 6 that binding gates nothing**, and pretending otherwise would be worse than
//! the gap: `/dev/draw` is bound unscoped into init's root namespace, every graphical client
//! inherits it, and resolves are classified by suffix with no caller identity. Namespace-based
//! gating needs *per-client namespaces*, and the process that constructs them is `desktop-shell`
//! — Milestone 7. See `TODO(manage-ungated)` and `docs/design/graphical-session.md` §3.

use libdraw::geom::{Point, Rect};
use librsproto::surface::{
    MgrPlace, MgrWindowRef, OP_MGR_CONFIGURE, OP_MGR_LOWER, OP_MGR_PLACE, OP_MGR_RAISE,
    OP_MGR_RAISE_ABOVE, OP_MGR_SET_FOCUS,
};

use crate::server::SurfaceError;
use crate::{StackError, WindowStack};

/// What a manager request did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MgrOutcome {
    /// Applied. `dirty` is the region to repaint, in screen coordinates.
    ///
    /// **`None` means "everything"** — the same convention `server::Outcome` uses, and the answer
    /// anything that cannot name its region must give. A restack is the case that needs it: which
    /// pixels change depends on every overlap in the stack, and computing that exactly would be a
    /// second compositor.
    Applied {
        /// The region this request changed, or `None` for "repaint everything".
        dirty: Option<Rect>,
    },
    /// Refused, with the reason.
    Failed(SurfaceError),
    /// A `Configure` the caller must forward to the window's client.
    ///
    /// Not performed here because it is a message to a *third* party — the manager asked, the
    /// client is told — and this module holds no channels. It is separate from `Applied` because
    /// a configure changes nothing on screen by itself: the client's next commit does.
    Configure {
        /// Which window's client to tell.
        window: u32,
        /// The suggested width.
        width: u32,
        /// The suggested height.
        height: u32,
        /// The suggested origin.
        origin: Point,
    },
}

/// Map a stack error to the wire error a manager sees.
fn refused(e: StackError) -> MgrOutcome {
    MgrOutcome::Failed(match e {
        StackError::NoSuchWindow => SurfaceError::NotFound,
        other => SurfaceError::Rejected(other),
    })
}

/// Handle one manager request.
///
/// `body` is the decoded rsproto payload. A malformed body is refused rather than guessed at, the
/// same rule the client dispatch follows.
pub fn dispatch(stack: &mut WindowStack, op: u16, body: &[u8]) -> MgrOutcome {
    match op {
        OP_MGR_PLACE => {
            let Some(req) = MgrPlace::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            match stack.place(req.window, Point::new(req.x, req.y)) {
                Ok(d) => MgrOutcome::Applied { dirty: Some(d.rect()) },
                Err(e) => refused(e),
            }
        }
        OP_MGR_RAISE | OP_MGR_LOWER | OP_MGR_RAISE_ABOVE | OP_MGR_SET_FOCUS => {
            let Some(req) = MgrWindowRef::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            let r = match op {
                OP_MGR_RAISE => stack.raise(req.window),
                OP_MGR_LOWER => stack.lower(req.window),
                OP_MGR_RAISE_ABOVE => stack.raise_above(req.window, req.other),
                // **Focus is a consequence of stacking, not a field.** `focus_candidate` is
                // topmost-focusable, so "give this window the keyboard" *is* "raise it" — and
                // keeping a separate focus field would be a second source of truth that could
                // disagree with the stack about who has it. Click-to-focus already relies on
                // this: `input.rs` raises and calls that the focus change.
                _ => stack.raise(req.window),
            };
            match r {
                // **A restack repaints everything.** Which pixels change depends on every
                // overlap in the stack; deriving the exact region would be a second compositor,
                // and a restack is a user-scale event rather than a per-frame one.
                Ok(()) => MgrOutcome::Applied { dirty: None },
                Err(e) => refused(e),
            }
        }
        OP_MGR_CONFIGURE => {
            let Some(req) = librsproto::surface::ConfigureEvent::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            if stack.window(req.window).is_none() {
                return MgrOutcome::Failed(SurfaceError::NotFound);
            }
            MgrOutcome::Configure {
                window: req.window,
                width: req.width,
                height: req.height,
                origin: Point::new(req.x, req.y),
            }
        }
        _ => MgrOutcome::Failed(SurfaceError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::surface::{ConfigureEvent, CreateWindowRequest, Role};

    /// A stack with `n` normal 8×8 windows, ids returned bottom-first.
    fn stack_with(n: usize) -> (WindowStack, alloc::vec::Vec<u32>) {
        let mut s = WindowStack::new();
        let ids = (0..n)
            .map(|_| {
                s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap()
            })
            .collect();
        (s, ids)
    }

    fn order(s: &WindowStack) -> alloc::vec::Vec<u32> {
        s.windows().iter().map(|w| w.id).collect()
    }

    fn place_body(window: u32, x: i32, y: i32) -> [u8; 12] {
        let mut b = [0u8; 12];
        MgrPlace { window, x, y }.write(&mut b).unwrap();
        b
    }

    fn ref_body(window: u32, other: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        MgrWindowRef { window, other }.write(&mut b).unwrap();
        b
    }

    #[test]
    fn a_manager_places_a_window_it_did_not_create() {
        // **The whole point of the channel.** Every op here names a window by id and none checks
        // ownership; the capability is holding the channel, not owning the window. A manager
        // that could only move its own windows would be an application.
        let (mut s, ids) = stack_with(1);
        let out = dispatch(&mut s, OP_MGR_PLACE, &place_body(ids[0], 40, 25));
        assert!(matches!(out, MgrOutcome::Applied { .. }));
        assert_eq!(s.window(ids[0]).unwrap().origin, Point::new(40, 25));
    }

    #[test]
    fn placing_an_uncommitted_window_reports_nothing_to_repaint() {
        // The manager's ordinary case: it places a window between the client's `CreateWindow`
        // and its first `Commit`, when the window is not on screen at all. Reporting a region
        // there would repaint on every window launch.
        let (mut s, ids) = stack_with(1);
        let MgrOutcome::Applied { dirty } = dispatch(&mut s, OP_MGR_PLACE, &place_body(ids[0], 5, 5))
        else {
            panic!("expected Applied")
        };
        let d = dirty.expect("a place names its region");
        assert!(d.size.w == 0 || d.size.h == 0, "an unmapped window is not on screen: {d:?}");
    }

    #[test]
    fn a_restack_repaints_everything_rather_than_guessing() {
        // Which pixels a restack changes depends on every overlap in the stack. `None` is this
        // crate's "I cannot name what changed", and deriving the exact region would be a second
        // compositor for a user-scale event.
        let (mut s, ids) = stack_with(3);
        for (op, body) in [
            (OP_MGR_RAISE, ref_body(ids[0], 0)),
            (OP_MGR_LOWER, ref_body(ids[0], 0)),
            (OP_MGR_SET_FOCUS, ref_body(ids[1], 0)),
        ] {
            assert_eq!(dispatch(&mut s, op, &body), MgrOutcome::Applied { dirty: None });
        }
    }

    #[test]
    fn raise_lower_and_raise_above_reorder_the_stack() {
        let (mut s, ids) = stack_with(3);
        assert_eq!(order(&s), ids);

        dispatch(&mut s, OP_MGR_RAISE, &ref_body(ids[0], 0));
        assert_eq!(order(&s), [ids[1], ids[2], ids[0]]);

        dispatch(&mut s, OP_MGR_LOWER, &ref_body(ids[0], 0));
        assert_eq!(order(&s), [ids[0], ids[1], ids[2]]);

        dispatch(&mut s, OP_MGR_RAISE_ABOVE, &ref_body(ids[0], ids[1]));
        assert_eq!(order(&s), [ids[1], ids[0], ids[2]], "just above, not to the top");
    }

    #[test]
    fn set_focus_is_a_raise_because_focus_is_a_consequence_of_stacking() {
        // Not a separate field: `focus_candidate` is topmost-focusable, so a focus field would
        // be a second source of truth that could disagree with the stack. Click-to-focus already
        // works this way.
        let (mut s, ids) = stack_with(3);
        assert_eq!(s.focus_candidate(), Some(ids[2]));
        dispatch(&mut s, OP_MGR_SET_FOCUS, &ref_body(ids[0], 0));
        assert_eq!(s.focus_candidate(), Some(ids[0]));
        assert_eq!(*order(&s).last().unwrap(), ids[0], "and it is on top, which is why");
    }

    #[test]
    fn a_configure_is_handed_back_rather_than_applied() {
        // It is a message to a *third* party — the manager asked, the client is told — and it
        // changes nothing on screen by itself: the client's next commit does.
        let (mut s, ids) = stack_with(1);
        let mut body = [0u8; 20];
        ConfigureEvent { window: ids[0], width: 300, height: 200, x: 10, y: 20 }
            .write(&mut body)
            .unwrap();
        assert_eq!(
            dispatch(&mut s, OP_MGR_CONFIGURE, &body),
            MgrOutcome::Configure {
                window: ids[0],
                width: 300,
                height: 200,
                origin: Point::new(10, 20),
            },
        );
        let w = s.window(ids[0]).unwrap();
        assert_eq!(w.size, (8, 8), "the stack is untouched until the client commits");
        assert_eq!(w.origin, Point::new(0, 0), "and it is not a placement in disguise");
    }

    #[test]
    fn every_op_refuses_a_window_that_does_not_exist() {
        // Stated over the set rather than for one op: a rule about all of them wants a test
        // over all of them, and the `Configure` arm reaches the stack by a different path from
        // the others.
        let (mut s, _) = stack_with(1);
        let mut cfg = [0u8; 20];
        ConfigureEvent { window: 99, width: 1, height: 1, x: 0, y: 0 }.write(&mut cfg).unwrap();
        let cases: [(u16, &[u8]); 6] = [
            (OP_MGR_PLACE, &place_body(99, 0, 0)),
            (OP_MGR_RAISE, &ref_body(99, 0)),
            (OP_MGR_LOWER, &ref_body(99, 0)),
            (OP_MGR_RAISE_ABOVE, &ref_body(99, 1)),
            (OP_MGR_SET_FOCUS, &ref_body(99, 0)),
            (OP_MGR_CONFIGURE, &cfg),
        ];
        for (op, body) in cases {
            assert_eq!(
                dispatch(&mut s, op, body),
                MgrOutcome::Failed(SurfaceError::NotFound),
                "op {op:#06x} accepted a window that does not exist",
            );
        }
    }

    #[test]
    fn a_truncated_body_is_refused_rather_than_read_short() {
        let (mut s, _) = stack_with(1);
        for (op, len) in [(OP_MGR_PLACE, 11usize), (OP_MGR_RAISE, 7), (OP_MGR_CONFIGURE, 19)] {
            let body = alloc::vec![0u8; len];
            assert_eq!(
                dispatch(&mut s, op, &body),
                MgrOutcome::Failed(SurfaceError::Malformed),
                "op {op:#06x} accepted {len} bytes",
            );
        }
        assert_eq!(dispatch(&mut s, 0x09FF, &[]), MgrOutcome::Failed(SurfaceError::Malformed));
    }
}
