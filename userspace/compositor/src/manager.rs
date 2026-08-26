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
//! — closed in M7 Part E, where `desktop-shell` binds `/dev/draw/new` alone into each
//! application namespace. See `docs/architecture/graphical-session.md` §3.

use libdraw::geom::{Point, Rect};
use librsproto::surface::{
    MgrDesktop, MgrPlace, MgrWindowRef, MgrWindowValue, OP_MGR_CONFIGURE, OP_MGR_LOWER,
    OP_MGR_PLACE, OP_MGR_RAISE, OP_MGR_RAISE_ABOVE, OP_MGR_SET_CURRENT_DESKTOP,
    OP_MGR_SET_FOCUS, OP_MGR_SET_MINIMIZED, OP_MGR_SET_WINDOW_DESKTOP,
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
        /// The window the request named.
        ///
        /// Carried rather than left for the caller to re-decode: acting on a window is what
        /// releases its held initial `Configure` (M6 B4), and reading the id back out of the
        /// request body would depend on every manager op happening to put it at offset 0 —
        /// true today, and exactly the kind of coincidence that stops being true quietly.
        window: u32,
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
        // The spec publishes `Malformed` for this: a current desktop of `STICKY_DESKTOP` is a
        // request that cannot mean anything, not a request the compositor declined to serve.
        StackError::StickyIsNotADesktop => SurfaceError::Malformed,
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
                Ok(d) => MgrOutcome::Applied { window: req.window, dirty: Some(d.rect()) },
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
                Ok(()) => MgrOutcome::Applied { window: req.window, dirty: None },
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
        OP_MGR_SET_WINDOW_DESKTOP | OP_MGR_SET_MINIMIZED => {
            let Some(req) = MgrWindowValue::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            let r = if op == OP_MGR_SET_WINDOW_DESKTOP {
                stack.set_window_desktop(req.window, req.value)
            } else {
                stack.set_minimized(req.window, req.value != 0)
            };
            match r {
                // **`dirty` is `None` when it changed, and `Some(empty)` when it did not.** A
                // window appearing or disappearing changes pixels wherever it overlapped
                // anything, which is the same "cannot name the region" case a restack is. But
                // moving a window between two desktops that are both not the current one — or
                // minimizing one that is already hidden — changes nothing on screen, and a full
                // repaint per such request would make a shell that tidies windows in the
                // background repaint the screen for each one.
                Ok(true) => MgrOutcome::Applied { window: req.window, dirty: None },
                Ok(false) => {
                    MgrOutcome::Applied { window: req.window, dirty: Some(Rect::new(0, 0, 0, 0)) }
                }
                Err(e) => refused(e),
            }
        }
        OP_MGR_SET_CURRENT_DESKTOP => {
            let Some(req) = MgrDesktop::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            match stack.set_current_desktop(req.desktop) {
                // **`window: 0`, which is no window.** Every other manager request names one and
                // `Applied.window` exists to release that window's held initial `Configure`;
                // this request names none, so it releases none. Zero is never a window id —
                // `WindowStack::new` starts `next_id` at 1 — so the caller's `release_configure`
                // finds nothing and does nothing, which is the correct behaviour rather than a
                // tolerated one.
                Ok(true) => MgrOutcome::Applied { window: 0, dirty: None },
                Ok(false) => {
                    MgrOutcome::Applied { window: 0, dirty: Some(Rect::new(0, 0, 0, 0)) }
                }
                Err(e) => refused(e),
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
                let id =
                    s.create(&CreateWindowRequest::new(8, 8, Role::Normal))
                        .unwrap();
                // Configured: a manager acts on windows that are on screen, and since B4
                // `focus_candidate` does not consider one that is not.
                s.mark_configured(id);
                id
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
        let MgrOutcome::Applied { dirty, .. } = dispatch(&mut s, OP_MGR_PLACE, &place_body(ids[0], 5, 5))
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
            assert!(matches!(dispatch(&mut s, op, &body), MgrOutcome::Applied { dirty: None, .. }));
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

    fn value_body(window: u32, value: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        MgrWindowValue { window, value }.write(&mut b).unwrap();
        b
    }

    #[test]
    fn the_desktop_requests_reach_the_stack_and_report_whether_the_screen_changed() {
        let (mut s, ids) = stack_with(1);
        let w = ids[0];

        // Off the current desktop: the screen changed, so the whole of it must be repainted —
        // which pixels changed depends on every overlap, the same case a restack is.
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_WINDOW_DESKTOP, &value_body(w, 2)),
            MgrOutcome::Applied { window: w, dirty: None },
        );
        assert_eq!(s.window(w).unwrap().desktop, 2);

        // Between two desktops that are both hidden: nothing on screen changed, and an empty
        // rectangle is how this codebase already says "repaint nothing".
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_WINDOW_DESKTOP, &value_body(w, 3)),
            MgrOutcome::Applied { window: w, dirty: Some(Rect::new(0, 0, 0, 0)) },
        );

        // Minimize takes any non-zero value as true, as the spec publishes.
        dispatch(&mut s, OP_MGR_SET_WINDOW_DESKTOP, &value_body(w, 1));
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_MINIMIZED, &value_body(w, 7)),
            MgrOutcome::Applied { window: w, dirty: None },
        );
        assert!(s.window(w).unwrap().minimized);
    }

    #[test]
    fn switching_the_current_desktop_names_no_window() {
        // **`window: 0` is the point.** `Applied.window` exists so the caller can release that
        // window's held initial `Configure`; this request names none, so it must release none.
        // Zero is never a window id — `next_id` starts at 1 — so the caller looks for nothing
        // and finds nothing.
        let (mut s, _) = stack_with(1);
        let mut b = [0u8; 4];
        MgrDesktop { desktop: 5 }.write(&mut b).unwrap();
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_CURRENT_DESKTOP, &b),
            MgrOutcome::Applied { window: 0, dirty: None },
        );
        assert_eq!(s.current_desktop(), 5);

        // Switching to the desktop already current changes nothing on screen.
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_CURRENT_DESKTOP, &b),
            MgrOutcome::Applied { window: 0, dirty: Some(Rect::new(0, 0, 0, 0)) },
        );
    }

    #[test]
    fn a_current_desktop_of_zero_is_malformed_on_the_wire() {
        // The spec publishes `Malformed` rather than a refusal: a current desktop of
        // `STICKY_DESKTOP` is a request that cannot mean anything, not one the compositor
        // declined to serve. Checked here because `refused()` maps every other `StackError`
        // to `Rejected`, so this one relies on an explicit arm.
        let (mut s, _) = stack_with(1);
        let mut b = [0u8; 4];
        MgrDesktop { desktop: librsproto::surface::STICKY_DESKTOP }.write(&mut b).unwrap();
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_CURRENT_DESKTOP, &b),
            MgrOutcome::Failed(SurfaceError::Malformed),
        );
        assert_eq!(s.current_desktop(), 1, "and the current desktop is untouched");
    }

    #[test]
    fn the_new_requests_refuse_an_unknown_window_and_a_short_body() {
        let (mut s, _) = stack_with(1);
        for op in [OP_MGR_SET_WINDOW_DESKTOP, OP_MGR_SET_MINIMIZED] {
            assert_eq!(
                dispatch(&mut s, op, &value_body(999, 1)),
                MgrOutcome::Failed(SurfaceError::NotFound),
                "op {op:#06x} accepted a window that does not exist",
            );
            assert_eq!(
                dispatch(&mut s, op, &[0u8; 7]),
                MgrOutcome::Failed(SurfaceError::Malformed),
                "op {op:#06x} accepted 7 bytes",
            );
        }
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_CURRENT_DESKTOP, &[0u8; 3]),
            MgrOutcome::Failed(SurfaceError::Malformed),
        );
    }
}
