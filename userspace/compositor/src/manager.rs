//! The manager channel's request handling: pure, host-tested, no syscalls.
//!
//! The counterpart of [`server::dispatch`](crate::server::dispatch) for the manager role: a
//! client speaks about windows it created, a manager about any window at all.
//!
//! Nothing here checks ownership: the capability is who may hold the channel
//! (`/dev/draw/manage`), not what may be asked over it. That binding gates nothing while
//! `/dev/draw` is bound unscoped for every client; `desktop-shell` binds only `/dev/draw/new`
//! into application namespaces. See `docs/architecture/graphical-session.md` §3.

use libdraw::geom::{Point, Rect};
use libdraw::theme::Scheme;
use librsproto::surface::{
    MgrDesktop, MgrHotkey, MgrPlace, MgrScheme, MgrWindowRef, MgrWindowValue, OP_MGR_CLOSE, OP_MGR_CONFIGURE,
    OP_MGR_LOWER, OP_MGR_PLACE, OP_MGR_RAISE, OP_MGR_RAISE_ABOVE, OP_MGR_REGISTER_HOTKEY, OP_MGR_REGISTER_SNAP_ZONE,
    OP_MGR_SET_CURRENT_DESKTOP, OP_MGR_SET_FOCUS, OP_MGR_SET_MINIMIZED, OP_MGR_SET_SCHEME, OP_MGR_SET_WINDOW_DESKTOP,
    SCHEME_DARK, SCHEME_LIGHT,
};

use crate::server::SurfaceError;
use crate::{StackError, WindowStack};

/// What a manager request did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MgrOutcome {
    /// Applied. `dirty` is the region to repaint, in screen coordinates.
    ///
    /// `None` means "everything", as in `server::Outcome`; only `SetCurrentDesktop` and `SetScheme`
    /// give it, as both replace what is on screen wholesale. A restack is confined to the moved
    /// window's rectangle; see [`WindowStack::raise`](crate::WindowStack::raise).
    Applied {
        /// The window the request named, or `None` for a request that names none.
        ///
        /// Carried so the caller need not re-decode it: acting on a window releases its held
        /// initial `Configure`. An `Option`, not a zero sentinel, so no `next_id` invariant is
        /// load-bearing.
        window: Option<u32>,
        /// The region this request changed, or `None` for "repaint everything".
        dirty: Option<Rect>,
    },
    /// Refused, with the reason.
    Failed(SurfaceError),
    /// A chord the caller must give to the input router, which owns the table.
    RegisterHotkey(MgrHotkey),
    /// A snap zone for the input router; a duplicate zone id replaces, a duplicate chord id is
    /// refused.
    RegisterZone(librsproto::surface::MgrSnapZone),
    /// A `Configure` for the caller to forward to the window's client; this module holds no
    /// channels, and nothing changes on screen until the client commits.
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

fn refused(e: StackError) -> MgrOutcome {
    MgrOutcome::Failed(match e {
        StackError::NoSuchWindow => SurfaceError::NotFound,
        // The spec publishes `Malformed`: a sticky current desktop cannot mean anything.
        StackError::StickyIsNotADesktop => SurfaceError::Malformed,
        other => SurfaceError::Rejected(other),
    })
}

/// Handle one manager request; a malformed `body` is refused rather than guessed at.
pub fn dispatch(stack: &mut WindowStack, op: u16, body: &[u8]) -> MgrOutcome {
    match op {
        OP_MGR_PLACE => {
            let Some(req) = MgrPlace::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            match stack.place(req.window, Point::new(req.x, req.y)) {
                Ok(d) => MgrOutcome::Applied { window: Some(req.window), dirty: Some(d.rect()) },
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
                // Focus is a consequence of stacking: `focus_candidate` is the topmost focusable
                // window, so a separate focus field would be a second source of truth.
                _ => stack.raise(req.window),
            };
            match r {
                // A restack repaints only the moved window's rectangle; empty damage means
                // nothing changed.
                Ok(d) => MgrOutcome::Applied { window: Some(req.window), dirty: Some(d.rect()) },
                Err(e) => refused(e),
            }
        }
        OP_MGR_CLOSE => {
            let Some(req) = MgrWindowRef::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            if stack.window(req.window).is_none() {
                return MgrOutcome::Failed(SurfaceError::NotFound);
            }
            // Painted bounds of everything, read before it goes, so a shadow is repainted too.
            let before: alloc::vec::Vec<(u32, Rect)> =
                stack.windows().iter().map(|w| (w.id, w.painted_bounds())).collect();
            match stack.destroy(req.window) {
                // Same removal as a client's `DestroyWindow`, descendants included, so the damage
                // is the union of everything that vanished: a child is not bounded by its parent.
                Ok(()) => {
                    let mut dirty: Option<Rect> = None;
                    for (id, bounds) in before {
                        if stack.window(id).is_none() {
                            dirty = Some(match dirty {
                                Some(d) => crate::union(d, bounds),
                                None => bounds,
                            });
                        }
                    }
                    MgrOutcome::Applied {
                        window: Some(req.window),
                        dirty: Some(dirty.unwrap_or(Rect::new(0, 0, 0, 0))),
                    }
                }
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
                // `None` when the screen changed, `Some(empty)` when not (e.g. between two hidden
                // desktops), so background tidying does not repaint the screen.
                Ok(true) => MgrOutcome::Applied { window: Some(req.window), dirty: None },
                Ok(false) => MgrOutcome::Applied { window: Some(req.window), dirty: Some(Rect::new(0, 0, 0, 0)) },
                Err(e) => refused(e),
            }
        }
        OP_MGR_SET_CURRENT_DESKTOP => {
            let Some(req) = MgrDesktop::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            match stack.set_current_desktop(req.desktop) {
                // No window is named, so there is no held `Configure` to release.
                Ok(true) => MgrOutcome::Applied { window: None, dirty: None },
                Ok(false) => MgrOutcome::Applied { window: None, dirty: Some(Rect::new(0, 0, 0, 0)) },
                Err(e) => refused(e),
            }
        }
        OP_MGR_SET_SCHEME => {
            let scheme = match MgrScheme::read(body).map(|r| r.scheme) {
                Some(SCHEME_LIGHT) => Scheme::Light,
                Some(SCHEME_DARK) => Scheme::Dark,
                // An unknown scheme is refused, not rounded to the nearest.
                _ => return MgrOutcome::Failed(SurfaceError::Malformed),
            };
            if stack.set_scheme(scheme) {
                // Everything: every shadow on screen changed.
                MgrOutcome::Applied { window: None, dirty: None }
            } else {
                MgrOutcome::Applied { window: None, dirty: Some(Rect::new(0, 0, 0, 0)) }
            }
        }
        OP_MGR_REGISTER_HOTKEY => {
            let Some(hk) = MgrHotkey::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            // Handed back: the chord table is the input router's.
            MgrOutcome::RegisterHotkey(hk)
        }
        OP_MGR_REGISTER_SNAP_ZONE => {
            let Some(z) = librsproto::surface::MgrSnapZone::read(body) else {
                return MgrOutcome::Failed(SurfaceError::Malformed);
            };
            MgrOutcome::RegisterZone(z)
        }
        _ => MgrOutcome::Failed(SurfaceError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::surface::{ConfigureEvent, CreateWindowRequest, Role};

    fn stack_with(n: usize) -> (WindowStack, alloc::vec::Vec<u32>) {
        let mut s = WindowStack::new();
        let ids = (0..n)
            .map(|_| {
                let id = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
                // Configured: `focus_candidate` ignores unconfigured windows.
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
        // No op checks ownership: holding the channel is the capability.
        let (mut s, ids) = stack_with(1);
        let out = dispatch(&mut s, OP_MGR_PLACE, &place_body(ids[0], 40, 25));
        assert!(matches!(out, MgrOutcome::Applied { .. }));
        assert_eq!(s.window(ids[0]).unwrap().origin, Point::new(40, 25));
    }

    #[test]
    fn placing_an_uncommitted_window_reports_nothing_to_repaint() {
        // Placed between `CreateWindow` and the first `Commit`: not on screen, nothing to repaint.
        let (mut s, ids) = stack_with(1);
        let MgrOutcome::Applied { dirty, .. } = dispatch(&mut s, OP_MGR_PLACE, &place_body(ids[0], 5, 5)) else {
            panic!("expected Applied")
        };
        let d = dirty.expect("a place names its region");
        assert!(d.size.w == 0 || d.size.h == 0, "an unmapped window is not on screen: {d:?}");
    }

    #[test]
    fn a_restack_repaints_the_window_that_moved_and_nothing_else() {
        // Only the moved window's own rectangle can change, not the whole screen.
        let (mut s, ids) = stack_with(3);
        for (op, body, id) in [
            (OP_MGR_RAISE, ref_body(ids[0], 0), ids[0]),
            (OP_MGR_LOWER, ref_body(ids[0], 0), ids[0]),
            (OP_MGR_SET_FOCUS, ref_body(ids[1], 0), ids[1]),
        ] {
            let want = s.window(id).expect("in the stack").painted_bounds();
            assert!(want.size.w > 8, "the premise: the damage includes the shadow");
            let MgrOutcome::Applied { dirty, .. } = dispatch(&mut s, op, &body) else { panic!("expected Applied") };
            assert_eq!(dirty, Some(want), "op {op:#06x} must name the window it moved");
        }
    }

    #[test]
    fn a_restack_that_moves_nothing_reports_nothing_to_repaint() {
        // Raising the topmost is the common case; empty damage (not `None`) repaints nothing.
        let (mut s, ids) = stack_with(3);
        let top = *ids.last().expect("three windows");
        let MgrOutcome::Applied { dirty, .. } = dispatch(&mut s, OP_MGR_RAISE, &ref_body(top, 0)) else {
            panic!("expected Applied")
        };
        let d = dirty.expect("named, not `everything`");
        assert!(d.size.w == 0 || d.size.h == 0, "already on top, so nothing moved: {d:?}");
        assert_eq!(order(&s), ids, "and the order is untouched");
    }

    #[test]
    fn close_destroys_a_window_the_caller_does_not_own_and_names_what_to_repaint() {
        // A manager can remove a window whose client has stopped answering.
        let (mut s, ids) = stack_with(2);
        let want = s.window(ids[0]).expect("in the stack").painted_bounds();
        assert!(want.origin.x < 0, "the premise: the region reaches outside the window");
        let MgrOutcome::Applied { window, dirty } = dispatch(&mut s, OP_MGR_CLOSE, &ref_body(ids[0], 0)) else {
            panic!("expected Applied")
        };
        assert_eq!(window, Some(ids[0]));
        assert_eq!(dirty, Some(want), "the rectangle it vacated, read before it went");
        assert_eq!(order(&s), [ids[1]], "and it is out of the stack");
        assert_eq!(s.take_removed(), vec![ids[0]], "recorded, so `WindowDestroyed` follows");

        assert!(matches!(
            dispatch(&mut s, OP_MGR_CLOSE, &ref_body(ids[0], 0)),
            MgrOutcome::Failed(SurfaceError::NotFound)
        ));
    }

    #[test]
    fn close_repaints_the_descendants_it_takes_with_it() {
        // Destroy is transitive and a popup is not bounded by its parent, so the damage is a union.
        let (mut s, ids) = stack_with(1);
        let parent = ids[0];
        let popup = s.create(&CreateWindowRequest::at(8, 8, Role::Popup { parent }, 100, 100)).unwrap();
        s.mark_configured(popup);
        let want = crate::union(s.window(parent).unwrap().painted_bounds(), s.window(popup).unwrap().painted_bounds());

        let MgrOutcome::Applied { dirty, .. } = dispatch(&mut s, OP_MGR_CLOSE, &ref_body(parent, 0)) else {
            panic!("expected Applied")
        };
        assert!(s.window(popup).is_none(), "the popup went with its parent");
        assert_eq!(dirty, Some(want), "the union of everything that vanished, not just the named window");
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
        let (mut s, ids) = stack_with(3);
        assert_eq!(s.focus_candidate(), Some(ids[2]));
        dispatch(&mut s, OP_MGR_SET_FOCUS, &ref_body(ids[0], 0));
        assert_eq!(s.focus_candidate(), Some(ids[0]));
        assert_eq!(*order(&s).last().unwrap(), ids[0], "and it is on top, which is why");
    }

    #[test]
    fn a_configure_is_handed_back_rather_than_applied() {
        let (mut s, ids) = stack_with(1);
        let mut body = [0u8; 20];
        ConfigureEvent { window: ids[0], width: 300, height: 200, x: 10, y: 20 }.write(&mut body).unwrap();
        assert_eq!(
            dispatch(&mut s, OP_MGR_CONFIGURE, &body),
            MgrOutcome::Configure { window: ids[0], width: 300, height: 200, origin: Point::new(10, 20) },
        );
        let w = s.window(ids[0]).unwrap();
        assert_eq!(w.size, (8, 8), "the stack is untouched until the client commits");
        assert_eq!(w.origin, Point::new(0, 0), "and it is not a placement in disguise");
    }

    #[test]
    fn every_op_refuses_a_window_that_does_not_exist() {
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

        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_WINDOW_DESKTOP, &value_body(w, 2)),
            MgrOutcome::Applied { window: Some(w), dirty: None },
        );
        assert_eq!(s.window(w).unwrap().desktop, 2);

        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_WINDOW_DESKTOP, &value_body(w, 3)),
            MgrOutcome::Applied { window: Some(w), dirty: Some(Rect::new(0, 0, 0, 0)) },
        );

        dispatch(&mut s, OP_MGR_SET_WINDOW_DESKTOP, &value_body(w, 1));
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_MINIMIZED, &value_body(w, 7)),
            MgrOutcome::Applied { window: Some(w), dirty: None },
        );
        assert!(s.window(w).unwrap().minimized);
    }

    #[test]
    fn switching_the_current_desktop_names_no_window() {
        // `Applied.window` is `None`: there is no held `Configure` to release.
        let (mut s, _) = stack_with(1);
        let mut b = [0u8; 4];
        MgrDesktop { desktop: 5 }.write(&mut b).unwrap();
        assert_eq!(dispatch(&mut s, OP_MGR_SET_CURRENT_DESKTOP, &b), MgrOutcome::Applied { window: None, dirty: None },);
        assert_eq!(s.current_desktop(), 5);

        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_CURRENT_DESKTOP, &b),
            MgrOutcome::Applied { window: None, dirty: Some(Rect::new(0, 0, 0, 0)) },
        );
    }

    #[test]
    fn a_current_desktop_of_zero_is_malformed_on_the_wire() {
        // The spec publishes `Malformed`; `refused()` needs an explicit arm for it.
        let (mut s, _) = stack_with(1);
        let mut b = [0u8; 4];
        MgrDesktop { desktop: librsproto::surface::STICKY_DESKTOP }.write(&mut b).unwrap();
        assert_eq!(dispatch(&mut s, OP_MGR_SET_CURRENT_DESKTOP, &b), MgrOutcome::Failed(SurfaceError::Malformed),);
        assert_eq!(s.current_desktop(), 1, "and the current desktop is untouched");
    }

    #[test]
    fn a_scheme_repaints_everything_once_and_an_unknown_one_is_refused() {
        let (mut s, _) = stack_with(1);
        let body = |scheme: u32| {
            let mut b = [0u8; 4];
            MgrScheme { scheme }.write(&mut b).unwrap();
            b
        };
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_SCHEME, &body(SCHEME_DARK)),
            MgrOutcome::Applied { window: None, dirty: None },
            "a change repaints the whole screen"
        );
        assert_eq!(s.scheme(), Scheme::Dark);
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_SCHEME, &body(SCHEME_DARK)),
            MgrOutcome::Applied { window: None, dirty: Some(Rect::new(0, 0, 0, 0)) },
            "the same scheme again changes nothing"
        );
        for bad in [&body(2)[..], &body(u32::MAX)[..], &[0u8; 3][..]] {
            assert_eq!(
                dispatch(&mut s, OP_MGR_SET_SCHEME, bad),
                MgrOutcome::Failed(SurfaceError::Malformed),
                "{bad:?} was taken as a scheme"
            );
        }
        assert_eq!(s.scheme(), Scheme::Dark, "and a refusal leaves the scheme alone");
        assert_eq!(
            dispatch(&mut s, OP_MGR_SET_SCHEME, &body(SCHEME_LIGHT)),
            MgrOutcome::Applied { window: None, dirty: None },
        );
        assert_eq!(s.scheme(), Scheme::Light);
    }

    #[test]
    fn registering_a_chord_is_handed_back_rather_than_applied() {
        let (mut s, _) = stack_with(1);
        let hk = MgrHotkey { id: 3, mods: 8, code: 59 };
        let mut b = [0u8; 8];
        hk.write(&mut b).unwrap();
        assert_eq!(
            dispatch(&mut s, OP_MGR_REGISTER_HOTKEY, &b),
            MgrOutcome::RegisterHotkey(hk),
            "the chord is passed through unchanged, mods and code included"
        );
        assert_eq!(
            dispatch(&mut s, OP_MGR_REGISTER_HOTKEY, &b[..7]),
            MgrOutcome::Failed(SurfaceError::Malformed),
            "7 bytes is not a chord"
        );
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
