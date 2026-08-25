//! Request dispatch — the server's decision-making, with no IPC in it.
//!
//! Shaped like `fs-server-ext4`'s `serve`: take a decoded request and a reply buffer,
//! decide, write the reply. The IPC loop, the framebuffer, and mapping the client's
//! transferred `MemoryObject`s are the bin's job; everything here is a pure function of
//! (connection, request, [`WindowStack`]) and host-tests without a kernel.
//!
//! ## Connections are the security boundary
//!
//! `docs/spec/rsproto-surface-ops.md` requires that **window ids are scoped to the
//! connection that created them**: a client may only name its own windows, and naming
//! another's is `NotFound`, exactly as if the id did not exist.
//!
//! That rule cannot live in [`WindowStack`], which is deliberately compositor-wide — a
//! desktop shell holding `/dev/draw` with broader rights has to be able to address any
//! window. So ownership is tracked here, per connection, and checked **before** dispatch
//! reaches the stack. Without it, holding `/dev/draw` — which the spec makes the whole of
//! the authority to *create* windows — would also be the authority to destroy anyone
//! else's.

use alloc::vec::Vec;

use librsproto::surface::{
    ConfigureEvent, OP_ATTACH_BUFFER, OP_COMMIT, OP_CREATE_WINDOW, OP_DESTROY_WINDOW,
    OP_SET_TITLE,
    build_create_window_reply,
    parse_attach_buffer_request, parse_commit_request, parse_create_window_request,
    parse_destroy_window_request,
};

use libdraw::geom::Rect;

use crate::{MAX_WINDOWS_PER_CONNECTION, StackError, WindowStack, union};

/// A client connection: the identity a request arrives on, and the windows it owns.
#[derive(Clone, Debug, Default)]
pub struct Connection {
    /// Windows created over this connection, in creation order.
    owned: Vec<u32>,
    /// Rejections logged for this connection so far.
    ///
    /// Per connection rather than per process so that one noisy client cannot spend the
    /// whole machine's diagnostic budget — `Connection::new` resets it, and a session slot
    /// is reused only by a different client.
    pub rejections_logged: u32,
}

impl Connection {
    /// A connection owning nothing.
    pub fn new() -> Self {
        Self { owned: Vec::new(), rejections_logged: 0 }
    }

    /// Whether this connection may name `window`.
    pub fn owns(&self, window: u32) -> bool {
        self.owned.contains(&window)
    }

    /// Windows this connection owns.
    pub fn owned(&self) -> &[u32] {
        &self.owned
    }
}

/// What a dispatched request produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A reply body of this length was written.
    Reply(usize),
    /// A window was created: reply with `reply_len` bytes, **then** send this `Configure`.
    ///
    /// Two messages for one request, which no other op needs, so it is a variant rather than a
    /// field on [`Reply`](Outcome::Reply). The order is the contract: a client blocked in
    /// `Window::new` reads the reply for the id and then waits for the configure, and a
    /// compositor that sent them the other way round would deadlock every client at startup.
    ///
    /// **The configure is what makes the window compositable** — see
    /// [`WindowStack::mark_configured`]. Until it arrives the client has no size it is entitled
    /// to commit at, which is the ordering that lets a manager place a window before it is seen.
    Created {
        /// Length of the `CreateWindow` reply body already written.
        reply_len: usize,
        /// The configure to send immediately after it.
        configure: ConfigureEvent,
    },
    /// Applied with no reply body.
    Applied {
        /// The `(window, buffer)` to release to the client, if any — **the buffer that
        /// left the screen**, never the one that arrived.
        release: Option<(u32, u32)>,
        /// The screen region this request changed, in screen coordinates.
        ///
        /// **`None` means "everything"**, which is the safe answer and the one anything that
        /// cannot name its region must give. `Some(empty)` means nothing changed on screen —
        /// attaching a buffer, for instance, which a later commit will display.
        ///
        /// This exists because the compositor recomposited the **whole screen on every
        /// request** until 2026-08-12, on a comment from Milestone 2 that said damage-bounded
        /// repaint was "an optimisation the protocol already carries and this does not yet
        /// exploit — a full repaint is always correct, and this milestone has one client".
        /// The premise expired: with three clients and an 812×480 terminal window, a boot
        /// spent a whole CPU compositing, which `test-harness`'s idle-occupancy check caught
        /// as a starved core (M5 Part B).
        dirty: Option<Rect>,
    },
    /// A window was renamed; the manager needs telling.
    ///
    /// A variant rather than an [`Applied`](Outcome::Applied) with a flag, because a title
    /// changes nothing on screen — the compositor draws no titles — and folding it into the
    /// damage-carrying variant would invite a repaint for a request that dirtied nothing.
    TitleSet {
        /// The renamed window. Its new title is on the [`Window`](crate::Window).
        window: u32,
    },
    /// The request was rejected.
    Failed(SurfaceError),
}

/// Why a request was refused.
///
/// Deliberately coarse where it faces the client: a window belonging to another
/// connection is [`SurfaceError::NotFound`], the same answer as one that never existed, so
/// the reply cannot be used to probe for other clients' window ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurfaceError {
    /// Undecodable body for this op.
    Malformed,
    /// The op is not one this server implements.
    Unsupported,
    /// No such window *for this connection* — including one that exists but belongs to
    /// somebody else.
    NotFound,
    /// The request was well-formed but could not be applied.
    Rejected(StackError),
}

/// Whether an over-long title has already been reported.
///
/// Single-threaded, like every other counter in this server.
static mut TITLE_TRUNCATED_LOGGED: bool = false;

/// Report the **first** over-long title and then stay quiet.
///
/// Once, not per occurrence: a client that sends a long title once usually sends it on every
/// update, and the cap is a normal outcome rather than a fault — the alternative is a serial
/// write on a request path, which is what a per-motion log cost `check-input` on 2026-08-24.
/// One line is enough to explain a title that came back shorter than it went out.
fn note_title_truncated(len: usize) {
    // SAFETY: single-threaded server; the same access pattern as the input diagnostics.
    unsafe {
        if TITLE_TRUNCATED_LOGGED {
            return;
        }
        TITLE_TRUNCATED_LOGGED = true;
    }
    let mut l = libkern::debug::Line::new();
    l.s(b"compositor: title of ")
        .u(len as u64)
        .s(b" bytes truncated to ")
        .u(librsproto::surface::MAX_TITLE as u64)
        .s(b" (further truncations not logged)");
    l.end();
}

/// Dispatch one Surface-category request arriving on `conn`.
///
/// `body` is the request body (the envelope is the caller's concern, as in `fs-server`).
/// A reply body, when there is one, is written into `reply`.
pub fn dispatch(
    conn: &mut Connection,
    stack: &mut WindowStack,
    op: u16,
    body: &[u8],
    reply: &mut [u8],
) -> Outcome {
    match op {
        OP_CREATE_WINDOW => {
            let Some(req) = parse_create_window_request(body) else {
                return Outcome::Failed(SurfaceError::Malformed);
            };
            // **Bounded per connection.** A client may legitimately hold several windows
            // since M6 C3 — a menu is a second window on its parent's session — and nothing
            // else bounded it: `conn.owned` and the stack are both plain `Vec`s. The old
            // one-window-per-connection API was the only thing keeping this finite, and it
            // was an accident of `libsurface` rather than a rule.
            if conn.owned.len() >= MAX_WINDOWS_PER_CONNECTION {
                return Outcome::Failed(SurfaceError::Rejected(StackError::TooManyWindows));
            }
            // A parent must be one of *this* connection's windows. Otherwise a client
            // could parent a popup onto a stranger's window and have it destroyed when
            // that window closes — or learn that the id exists.
            if let Some(parent) = parent_of(&req.role)
                && !conn.owns(parent)
            {
                return Outcome::Failed(SurfaceError::NotFound);
            }
            match stack.create(&req) {
                Ok(id) => {
                    conn.owned.push(id);
                    match build_create_window_reply(reply, id) {
                        Some(n) => {
                            // **The default answer: the client's own request echoed back.**
                            // This is what the client is told when nobody has an opinion — no
                            // manager attached, or one that did not answer in time.
                            //
                            // A manager's answer does *not* land here. The bin holds this
                            // record when a manager is attached and releases it with the
                            // geometry as it stands once the manager has acted (M6 B4), so
                            // this library half stays free of both the manager and the clock.
                            let w = stack.window(id).expect("just created");
                            Outcome::Created {
                                reply_len: n,
                                configure: ConfigureEvent {
                                    window: id,
                                    width: req.width,
                                    height: req.height,
                                    x: w.origin.x,
                                    y: w.origin.y,
                                },
                            }
                        }
                        None => Outcome::Failed(SurfaceError::Malformed),
                    }
                }
                Err(e) => Outcome::Failed(SurfaceError::Rejected(e)),
            }
        }

        OP_ATTACH_BUFFER => {
            let Some(req) = parse_attach_buffer_request(body) else {
                return Outcome::Failed(SurfaceError::Malformed);
            };
            if !conn.owns(req.window) {
                return Outcome::Failed(SurfaceError::NotFound);
            }
            match stack.attach(&req) {
                // Attaching shows nothing: the buffer reaches the screen at the commit.
                Ok(()) => Outcome::Applied { release: None, dirty: Some(Rect::new(0, 0, 0, 0)) },
                Err(e) => Outcome::Failed(SurfaceError::Rejected(e)),
            }
        }

        OP_COMMIT => {
            let Some(req) = parse_commit_request(body) else {
                return Outcome::Failed(SurfaceError::Malformed);
            };
            if !conn.owns(req.window) {
                return Outcome::Failed(SurfaceError::NotFound);
            }
            // **Read before the commit.** `Window::bounds` reports the *committed* buffer's
            // geometry, so a window whose new buffer is smaller than its old one would have its
            // damage clipped to the new, smaller rectangle — and the band the old buffer
            // occupied and the new one does not would keep the old pixels until something
            // unrelated forced a repaint. Before damage-bounded repaint the unconditional
            // full recomposite covered that; now it has to be said (PR #192 review, finding 3).
            let was = stack.window(req.window).map(|w| w.bounds());
            match stack.commit(&req) {
                Ok(previous) => {
                    // **The damage the client sent**, translated into screen coordinates and
                    // clipped to the window it belongs to — old bounds unioned with new, so a
                    // shrunk window still repaints what it vacated.
                    //
                    // **The clip is a bound on work, not a barrier against a leak.** An earlier
                    // version of this comment claimed an unclipped rectangle would "repaint a
                    // neighbour's pixels from this window's buffer", which is not how
                    // compositing works here: `libdraw::compose` clears the damaged area and
                    // blits *every* surface clipped to its own bounds, so an over-large
                    // rectangle produces a correct recomposite of that area, just a needlessly
                    // large one. What a client does control is how much work each commit costs
                    // — unclipped, every commit is a full-screen recomposite, which is exactly
                    // the cost this change exists to remove (PR #192 review, finding 4).
                    let dirty = stack.window(req.window).map(|w| {
                        let local = Rect::new(
                            req.damage_x as i32,
                            req.damage_y as i32,
                            req.damage_w,
                            req.damage_h,
                        );
                        let bounds = w.bounds();
                        // **A window that changed shape repaints both shapes, whatever it
                        // said.** The vacated band cannot be in the client's damage — the
                        // client is describing its *new* buffer — and a rectangle cannot
                        // express "old minus new", so the union of the two is the tightest
                        // correct answer.
                        if let Some(before) = was.filter(|b| *b != bounds) {
                            return union(before, bounds);
                        }
                        let moved = Rect::new(
                            bounds.origin.x.saturating_add(local.origin.x),
                            bounds.origin.y.saturating_add(local.origin.y),
                            local.size.w,
                            local.size.h,
                        );
                        moved.intersect(&bounds).unwrap_or(Rect::new(0, 0, 0, 0))
                    });
                    Outcome::Applied { release: previous.map(|b| (req.window, b)), dirty }
                }
                Err(e) => Outcome::Failed(SurfaceError::Rejected(e)),
            }
        }

        OP_SET_TITLE => {
            let Some((id, title)) = librsproto::surface::title::read(body) else {
                return Outcome::Failed(SurfaceError::Malformed);
            };
            // Same ownership rule as every other op: a window belonging to another connection
            // answers `NotFound`, so a reply cannot be used to probe for other clients' ids.
            if !conn.owned.contains(&id) {
                return Outcome::Failed(SurfaceError::NotFound);
            }
            let Some(w) = stack.window_mut(id) else {
                return Outcome::Failed(SurfaceError::NotFound);
            };
            let kept = librsproto::surface::title::truncate_title(title);
            if kept.len() < title.len() {
                note_title_truncated(title.len());
            }
            // Unchanged titles do not become manager traffic: a client that re-sets the same
            // title on every frame is not hypothetical, and the outbox is a bounded queue that
            // discards its oldest when it fills.
            if w.title == kept {
                return Outcome::Applied { release: None, dirty: Some(Rect::new(0, 0, 0, 0)) };
            }
            w.title.clear();
            w.title.push_str(kept);
            Outcome::TitleSet { window: id }
        }

        OP_DESTROY_WINDOW => {
            let Some(window) = parse_destroy_window_request(body) else {
                return Outcome::Failed(SurfaceError::Malformed);
            };
            if !conn.owns(window) {
                return Outcome::Failed(SurfaceError::NotFound);
            }
            let before: Vec<(u32, Rect)> =
                stack.windows().iter().map(|w| (w.id, w.bounds())).collect();
            match stack.destroy(window) {
                Ok(()) => {
                    // Destroy is transitive, so this connection's descendants of `window`
                    // are gone from the stack too. Drop every id it no longer holds, or
                    // the connection keeps claiming ownership of windows that do not
                    // exist — and a later id reuse would hand it someone else's window.
                    conn.owned.retain(|id| stack.window(*id).is_some());
                    // **The union of every window that vanished**, not just the named one:
                    // destroy is transitive, so a popup's children go with it and their pixels
                    // have to be repainted too.
                    let mut dirty: Option<Rect> = None;
                    for (id, bounds) in before {
                        if stack.window(id).is_none() {
                            dirty = Some(match dirty {
                                Some(d) => union(d, bounds),
                                None => bounds,
                            });
                        }
                    }
                    Outcome::Applied { release: None, dirty: Some(dirty.unwrap_or(Rect::new(0, 0, 0, 0))) }
                }
                Err(e) => Outcome::Failed(SurfaceError::Rejected(e)),
            }
        }

        _ => Outcome::Failed(SurfaceError::Unsupported),
    }
}

/// Drop everything a connection owns — what the bin calls when a client disconnects.
///
/// A client that exits without destroying its windows must not leave them on screen
/// forever. Destroy is transitive, so a parent takes its popups with it and the remaining
/// ids simply resolve to nothing.
pub fn disconnect(conn: &mut Connection, stack: &mut WindowStack) {
    for id in core::mem::take(&mut conn.owned) {
        // Already gone if an ancestor took it; that is not an error.
        let _ = stack.destroy(id);
    }
}

/// The parent a role names, if it names one.
fn parent_of(role: &librsproto::surface::Role) -> Option<u32> {
    match role {
        librsproto::surface::Role::Popup { parent }
        | librsproto::surface::Role::Dialog { parent } => Some(*parent),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert an outcome applied, with the given release and whatever region.
    ///
    /// `dirty` has tests of its own below. Spelling a rectangle into every assertion here
    /// would make each one about two things, and the release is what they were written for.
    #[track_caller]
    fn assert_applied(o: Outcome, release: Option<(u32, u32)>) {
        match o {
            Outcome::Applied { release: r, .. } => assert_eq!(r, release, "release"),
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    /// The region an outcome reports dirtying.
    #[track_caller]
    fn dirty_of(o: Outcome) -> Option<Rect> {
        match o {
            Outcome::Applied { dirty, .. } => dirty,
            other => panic!("expected Applied, got {other:?}"),
        }
    }
    use librsproto::surface::{
        ATTACH_BUFFER_REQUEST_LEN, AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge,
        Role, SURFACE_FORMAT_XRGB8888, build_attach_buffer_request, build_commit_request,
        build_create_window_request, build_destroy_window_request, parse_create_window_reply,
    };

    pub(super) fn create(
        conn: &mut Connection,
        stack: &mut WindowStack,
        role: Role,
    ) -> Result<u32, SurfaceError> {
        create_sized(conn, stack, role, 8, 8)
    }

    /// `create` at a stated size. Most tests do not care and use the 8×8 default; the ones
    /// that do care are the ones where a child must *not* be a subset of its parent.
    fn create_sized(
        conn: &mut Connection,
        stack: &mut WindowStack,
        role: Role,
        width: u32,
        height: u32,
    ) -> Result<u32, SurfaceError> {
        let mut body = [0u8; 32];
        let n = build_create_window_request(
            &mut body,
            &CreateWindowRequest::new(width, height, role),
        )
        .unwrap();
        let mut reply = [0u8; 32];
        match dispatch(conn, stack, OP_CREATE_WINDOW, &body[..n], &mut reply) {
            // Create answers with a reply *and* the window's first `Configure` — the ordering
            // that lets a manager place a window before it is seen. Most tests care only about
            // the id; `create_configured` is for the ones that care about the configure.
            Outcome::Created { reply_len, .. } => {
                let id = parse_create_window_reply(&reply[..reply_len]).unwrap();
                // **Standing in for the bin**, which marks a window configured the instant it
                // sends the first `Configure` — immediately, when no manager is attached, which
                // is the case every test in this module models. Without it these windows are
                // held, and since B4 a held window is neither composited, focusable, nor
                // clickable.
                stack.mark_configured(id);
                Ok(id)
            }
            Outcome::Failed(e) => Err(e),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn attach(conn: &mut Connection, stack: &mut WindowStack, window: u32, buffer: u32) -> Outcome {
        attach_sized(conn, stack, window, buffer, 8, 8)
    }

    /// `attach` at a stated size — see [`create_sized`].
    fn attach_sized(
        conn: &mut Connection,
        stack: &mut WindowStack,
        window: u32,
        buffer: u32,
        width: u32,
        height: u32,
    ) -> Outcome {
        let mut body = [0u8; ATTACH_BUFFER_REQUEST_LEN];
        let n = build_attach_buffer_request(
            &mut body,
            &AttachBufferRequest {
                window,
                buffer,
                width,
                height,
                pitch: width * 4,
                format: SURFACE_FORMAT_XRGB8888,
            },
        )
        .unwrap();
        let mut reply = [0u8; 8];
        dispatch(conn, stack, OP_ATTACH_BUFFER, &body[..n], &mut reply)
    }

    /// `commit` with an explicit damage rectangle.
    fn commit_damage(
        conn: &mut Connection,
        stack: &mut WindowStack,
        window: u32,
        buffer: u32,
        d: (u32, u32, u32, u32),
    ) -> Outcome {
        let mut body = [0u8; 32];
        let n = build_commit_request(
            &mut body,
            &CommitRequest {
                window,
                buffer,
                damage_x: d.0,
                damage_y: d.1,
                damage_w: d.2,
                damage_h: d.3,
            },
        )
        .unwrap();
        let mut reply = [0u8; 8];
        dispatch(conn, stack, OP_COMMIT, &body[..n], &mut reply)
    }

    fn commit(conn: &mut Connection, stack: &mut WindowStack, window: u32, buffer: u32) -> Outcome {
        let mut body = [0u8; 32];
        let n = build_commit_request(
            &mut body,
            &CommitRequest {
                window,
                buffer,
                damage_x: 0,
                damage_y: 0,
                damage_w: 8,
                damage_h: 8,
            },
        )
        .unwrap();
        let mut reply = [0u8; 8];
        dispatch(conn, stack, OP_COMMIT, &body[..n], &mut reply)
    }

    fn destroy(conn: &mut Connection, stack: &mut WindowStack, window: u32) -> Outcome {
        let mut body = [0u8; 8];
        let n = build_destroy_window_request(&mut body, window).unwrap();
        let mut reply = [0u8; 8];
        dispatch(conn, stack, OP_DESTROY_WINDOW, &body[..n], &mut reply)
    }

    #[test]
    fn a_connection_owns_the_windows_it_creates() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert!(a.owns(w));
        assert_eq!(a.owned(), &[w]);
    }

    #[test]
    fn one_connection_cannot_touch_another_s_window() {
        // The spec's rule. Holding `/dev/draw` is the authority to create windows; it must
        // not be the authority to destroy anyone else's.
        let mut stack = WindowStack::new();
        let (mut a, mut b) = (Connection::new(), Connection::new());
        let owned_by_a = create(&mut a, &mut stack, Role::Normal).unwrap();

        assert_eq!(attach(&mut b, &mut stack, owned_by_a, 0), Outcome::Failed(SurfaceError::NotFound));
        assert_eq!(commit(&mut b, &mut stack, owned_by_a, 0), Outcome::Failed(SurfaceError::NotFound));
        assert_eq!(destroy(&mut b, &mut stack, owned_by_a), Outcome::Failed(SurfaceError::NotFound));
        assert!(stack.window(owned_by_a).is_some(), "A's window survived B's attempts");
    }

    #[test]
    fn a_commit_dirties_the_damage_it_named_and_no_more() {
        // **The whole point of the field.** Until 2026-08-12 every request recomposited the
        // screen, and with an 812×480 terminal window in the stack that cost a permanently
        // busy CPU — caught by `test-harness`'s idle-occupancy check, which is the only thing
        // in the tree that could have caught it.
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, w, 0), None);
        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 0, (2, 3, 4, 5)));
        assert_eq!(d, Some(Rect::new(2, 3, 4, 5)));
    }

    #[test]
    fn a_commits_damage_is_clipped_to_its_own_window() {
        // A client is not trusted to bound its own rectangle. A damage larger than the window
        // would repaint a *neighbour's* pixels out of this window's buffer.
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, w, 0), None);
        // The window is 8×8 at the origin; the client claims 400×400 of damage.
        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 0, (0, 0, 400, 400)));
        assert_eq!(d, Some(Rect::new(0, 0, 8, 8)));
        // And damage entirely outside the window dirties nothing rather than something.
        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 0, (100, 100, 4, 4)));
        assert_eq!(d.map(|r| (r.size.w, r.size.h)), Some((0, 0)));
    }

    #[test]
    fn attaching_a_buffer_dirties_nothing() {
        // The buffer reaches the screen at the commit. A repaint here is work for a picture
        // that has not changed.
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        let d = dirty_of(attach(&mut a, &mut stack, w, 0)).expect("attach names a region");
        assert!(d.size.w == 0 || d.size.h == 0, "attach dirtied {d:?}");
    }

    #[test]
    fn destroying_dirties_what_vanished_including_a_child_bigger_than_its_parent() {
        // Destroy is transitive: a popup's children go with it, and their pixels have to be
        // repainted too. Naming only the window in the request would leave a child's pixels on
        // screen after it was gone.
        //
        // **The child is 16×16 to the parent's 8×8**, which is the whole point of the test. A
        // first version created one childless window and asserted its own bounds — so an
        // implementation that reported only the named window's rectangle passed the entire
        // file, and neither the transitive union nor `union()` itself was exercised (PR #192
        // review, finding 1). A child whose bounds are a subset of its parent's cannot tell
        // the two apart either.
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let parent = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, parent, 0), None);
        assert_applied(commit(&mut a, &mut stack, parent, 0), None);

        let popup = create_sized(&mut a, &mut stack, Role::Popup { parent }, 16, 16).unwrap();
        assert_applied(attach_sized(&mut a, &mut stack, popup, 0, 16, 16), None);
        assert_applied(commit_damage(&mut a, &mut stack, popup, 0, (0, 0, 16, 16)), None);
        assert_eq!(
            stack.window(popup).map(|w| w.bounds().size),
            Some(libdraw::geom::Size::new(16, 16)),
            "the premise: the child is larger than its parent",
        );

        let d = dirty_of(destroy(&mut a, &mut stack, parent)).expect("destroy names a region");
        assert!(stack.window(popup).is_none(), "the premise: destroy took the child too");
        assert_eq!(d, Rect::new(0, 0, 16, 16), "the union of both, not the parent's 8×8");
    }

    #[test]
    fn a_window_whose_buffer_shrinks_repaints_what_it_vacated() {
        // `Window::bounds` reports the *committed* buffer, so damage clipped after the commit
        // is clipped to the new, smaller rectangle — and the band the old buffer occupied and
        // the new one does not keeps the old pixels until something unrelated forces a full
        // repaint. Before damage-bounded repaint the unconditional recomposite covered it, so
        // this was a regression this PR introduced (PR #192 review, finding 3).
        //
        // Not reachable in-tree today — every window in the image is fixed-size and M6 owns
        // resize — which is exactly why it needs a test rather than a client to find it.
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, w, 0), None);
        assert_applied(commit(&mut a, &mut stack, w, 0), None);

        // A 4×4 buffer replaces the 8×8 one. The client honestly damages its whole new buffer.
        assert_applied(attach_sized(&mut a, &mut stack, w, 1, 4, 4), None);
        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 1, (0, 0, 4, 4)))
            .expect("a commit names a region");
        assert_eq!(d, Rect::new(0, 0, 8, 8), "only the new 4x4 was repainted, vacating a band");
    }

    #[test]
    fn another_connection_s_window_is_not_found_not_denied() {
        // The two answers must be indistinguishable, or the reply is an oracle for probing
        // which window ids exist.
        let mut stack = WindowStack::new();
        let (mut a, mut b) = (Connection::new(), Connection::new());
        let real = create(&mut a, &mut stack, Role::Normal).unwrap();
        let never_existed = 9999;
        assert_eq!(destroy(&mut b, &mut stack, real), destroy(&mut b, &mut stack, never_existed));
    }

    /// A connection cannot hold more than [`MAX_WINDOWS_PER_CONNECTION`] windows at once.
    ///
    /// Nothing bounded this before C3: `conn.owned` and the stack are plain `Vec`s, and the
    /// only thing keeping a client to one window was `libsurface`'s API owning the transport.
    /// A session type removes that accident, so the bound has to be real.
    #[test]
    fn a_connection_is_bounded_in_how_many_windows_it_may_hold() {
        let mut stack = WindowStack::new();
        let mut conn = Connection::new();
        for i in 0..MAX_WINDOWS_PER_CONNECTION {
            create(&mut conn, &mut stack, Role::Normal)
                .unwrap_or_else(|e| panic!("window {i} refused: {e:?}"));
        }
        assert_eq!(
            create(&mut conn, &mut stack, Role::Normal),
            Err(SurfaceError::Rejected(StackError::TooManyWindows)),
            "the one past the bound is refused, and says why"
        );

        // **Destroying one makes room again**, so the bound is on what is held rather than on
        // how many have ever been created — a client that opens and closes menus forever must
        // not eventually be unable to open another.
        let first = conn.owned()[0];
        assert!(matches!(destroy(&mut conn, &mut stack, first), Outcome::Applied { .. }));
        create(&mut conn, &mut stack, Role::Normal).expect("room again");

        // Another connection is unaffected: the bound is per connection, not global.
        let mut other = Connection::new();
        create(&mut other, &mut stack, Role::Normal).expect("a different client is not punished");
    }

    #[test]
    fn a_popup_cannot_be_parented_onto_another_connection_s_window() {
        let mut stack = WindowStack::new();
        let (mut a, mut b) = (Connection::new(), Connection::new());
        let owned_by_a = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_eq!(
            create(&mut b, &mut stack, Role::Popup { parent: owned_by_a }),
            Err(SurfaceError::NotFound),
            "otherwise closing A's window would destroy B's popup"
        );
    }

    #[test]
    fn a_create_moves_focus_and_is_not_an_applied() {
        // **The trap, stated where a host test can hold it shut.** `focus_candidate` is the
        // topmost focus-taking window and a create puts one on top, so focus moves on the
        // create itself — before the client has attached a buffer, let alone committed. But
        // create is the one op that answers with a window id, so it returns `Outcome::Reply`
        // and never goes near the `Applied` path.
        //
        // The compositor announced focus from inside that path, so a create told nobody: the
        // previously focused window kept blinking a caret it no longer owned while its keys
        // went to a window that had not painted (PR #184 review, finding 2). `serve_session`
        // now announces after every request whatever its outcome.
        //
        // **What this test does and does not hold shut.** It pins the *premise* — a create
        // is a `Reply`, and focus moves on it — so anyone who reads `Applied` as "the ops
        // that change things" is contradicted here. It does **not** catch the regression
        // itself: `announce_focus`'s call site is in the bin, and moving it back inside the
        // `Applied` arm leaves every assertion below still true. What catches that is the
        // boot gate's ordering check — a window's *first* event must be its focus change —
        // added for exactly this reason, because the gate as it stood passed with the bug
        // reintroduced.
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let first = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_eq!(stack.focus_candidate(), Some(first));

        let mut body = [0u8; 32];
        let n = build_create_window_request(
            &mut body,
            &CreateWindowRequest::new(8, 8, Role::Normal),
        )
        .unwrap();
        let mut reply = [0u8; 32];
        let outcome = dispatch(&mut a, &mut stack, OP_CREATE_WINDOW, &body[..n], &mut reply);
        assert!(
            !matches!(outcome, Outcome::Applied { .. }),
            "a create is not an Applied, so anything keyed on Applied misses it: {outcome:?}"
        );
        let Outcome::Created { reply_len, configure } = outcome else {
            panic!("unexpected {outcome:?}")
        };
        let second = parse_create_window_reply(&reply[..reply_len]).unwrap();
        assert_eq!(configure.window, second, "the configure names the window just created");

        assert_ne!(second, first);

        // **Since B4 the create alone is not enough: focus follows the configure.** A window
        // that is not on screen must not hold the keyboard, and one whose configure is held is
        // not on screen. The premise this test exists for survives — focus still moves without
        // an attach, a commit, or an `Applied` — but the step that moves it is the first
        // `Configure`, which the bin sends immediately when no manager is attached and holds
        // when one is (PR #218 review, finding 3).
        assert_eq!(
            stack.focus_candidate(),
            Some(first),
            "held: the new window is on top of the stack but is not on screen, so focus stays"
        );

        stack.mark_configured(second);
        assert_eq!(
            stack.focus_candidate(),
            Some(second),
            "configured: focus moved with no attach, no commit, no Applied anywhere"
        );
    }

    #[test]
    fn commit_reports_the_buffer_to_release() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, w, 0), None);
        assert_applied(attach(&mut a, &mut stack, w, 1), None);

        assert_applied(commit(&mut a, &mut stack, w, 0), None);
        // The buffer that left the screen, never the one that arrived.
        assert_applied(commit(&mut a, &mut stack, w, 1), Some((w, 0)));
    }

    #[test]
    fn destroying_a_parent_drops_the_descendants_from_the_connection_too() {
        // Otherwise the connection keeps claiming ids that no longer exist, and a later id
        // reuse hands it a window it does not own.
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        let menu = create(&mut a, &mut stack, Role::Popup { parent: w }).unwrap();
        let sub = create(&mut a, &mut stack, Role::Popup { parent: menu }).unwrap();
        assert_eq!(a.owned().len(), 3);

        assert_applied(destroy(&mut a, &mut stack, w), None);
        assert!(a.owned().is_empty(), "stale ids left on the connection: {:?}", a.owned());
        for gone in [w, menu, sub] {
            assert!(stack.window(gone).is_none());
        }
    }

    #[test]
    fn disconnecting_removes_everything_the_client_had_on_screen() {
        let mut stack = WindowStack::new();
        let (mut a, mut b) = (Connection::new(), Connection::new());
        let wa = create(&mut a, &mut stack, Role::Normal).unwrap();
        let menu = create(&mut a, &mut stack, Role::Popup { parent: wa }).unwrap();
        let wb = create(&mut b, &mut stack, Role::Normal).unwrap();

        disconnect(&mut a, &mut stack);
        assert!(stack.window(wa).is_none());
        assert!(stack.window(menu).is_none(), "a popup must not outlive its client");
        assert!(stack.window(wb).is_some(), "another client is untouched");
        assert!(a.owned().is_empty());
    }

    #[test]
    fn a_malformed_body_is_rejected_without_touching_the_stack() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let mut reply = [0u8; 32];
        for op in [OP_CREATE_WINDOW, OP_ATTACH_BUFFER, OP_COMMIT, OP_DESTROY_WINDOW] {
            assert_eq!(
                dispatch(&mut a, &mut stack, op, &[0u8; 2], &mut reply),
                Outcome::Failed(SurfaceError::Malformed),
                "op {op:#06x}"
            );
        }
        assert!(stack.windows().is_empty());
        assert!(a.owned().is_empty());
    }

    #[test]
    fn an_unknown_op_is_unsupported() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let mut reply = [0u8; 32];
        assert_eq!(
            dispatch(&mut a, &mut stack, 0x09FE, &[], &mut reply),
            Outcome::Failed(SurfaceError::Unsupported)
        );
    }

    #[test]
    fn a_panel_can_be_created_over_a_connection_like_any_other_window() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let p = create(&mut a, &mut stack, Role::Panel { dock: Edge::Top, reserve: 24 }).unwrap();
        assert!(a.owns(p));
        assert_eq!(stack.work_area(libdraw::geom::Rect::new(0, 0, 100, 50)),
            libdraw::geom::Rect::new(0, 24, 100, 26));
    }
}

#[cfg(test)]
mod title_tests {
    use super::tests::create;
    use super::*;
    use crate::Role;
    use librsproto::surface::{MAX_TITLE, title};

    /// Build a `SetTitle` body and dispatch it.
    fn set_title(
        conn: &mut Connection,
        stack: &mut WindowStack,
        window: u32,
        s: &str,
    ) -> Outcome {
        let mut body = alloc::vec![0u8; 4 + s.len()];
        let n = title::write(window, s, &mut body).expect("fits");
        let mut reply = [0u8; 32];
        dispatch(conn, stack, OP_SET_TITLE, &body[..n], &mut reply)
    }

    #[test]
    fn a_title_is_stored_and_the_manager_is_told() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_eq!(stack.window(w).unwrap().title, "", "a new window is unnamed");
        assert!(matches!(set_title(&mut a, &mut stack, w, "nxterm"), Outcome::TitleSet { window } if window == w));
        assert_eq!(stack.window(w).unwrap().title, "nxterm");
    }

    /// Re-setting the same title must not become manager traffic: a client that sets it every
    /// frame would otherwise push the bounded queue's older events out.
    #[test]
    fn an_unchanged_title_produces_no_manager_event() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert!(matches!(set_title(&mut a, &mut stack, w, "same"), Outcome::TitleSet { .. }));
        assert!(
            matches!(set_title(&mut a, &mut stack, w, "same"), Outcome::Applied { .. }),
            "the same title was announced twice"
        );
        // Negative control: a different title is announced.
        assert!(matches!(set_title(&mut a, &mut stack, w, "other"), Outcome::TitleSet { .. }));
    }

    /// The same ownership rule as every other op — and `NotFound` rather than a distinct
    /// error, so a reply cannot be used to probe for another client's window ids.
    #[test]
    fn another_connections_window_cannot_be_renamed() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let mut b = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert!(matches!(
            set_title(&mut b, &mut stack, w, "mine now"),
            Outcome::Failed(SurfaceError::NotFound)
        ));
        assert_eq!(stack.window(w).unwrap().title, "", "the window was renamed anyway");
        // A window that never existed answers the same thing.
        assert!(matches!(
            set_title(&mut b, &mut stack, w + 99, "ghost"),
            Outcome::Failed(SurfaceError::NotFound)
        ));
    }

    /// Stored truncated, so a window's cost stays finite however chatty its client is.
    #[test]
    fn an_over_long_title_is_stored_truncated() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        let long = alloc::string::String::from_utf8(alloc::vec![b'x'; MAX_TITLE + 50]).unwrap();
        assert!(matches!(set_title(&mut a, &mut stack, w, &long), Outcome::TitleSet { .. }));
        assert_eq!(stack.window(w).unwrap().title.len(), MAX_TITLE);
    }

    /// A body that is not UTF-8 is malformed, not a silently-empty title.
    #[test]
    fn a_malformed_body_is_refused() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        let mut reply = [0u8; 32];
        let bad = [w as u8, 0, 0, 0, 0xFF, 0xFE];
        assert!(matches!(
            dispatch(&mut a, &mut stack, OP_SET_TITLE, &bad, &mut reply),
            Outcome::Failed(SurfaceError::Malformed)
        ));
        // Too short to name a window.
        assert!(matches!(
            dispatch(&mut a, &mut stack, OP_SET_TITLE, &[0u8; 2], &mut reply),
            Outcome::Failed(SurfaceError::Malformed)
        ));
    }
}

