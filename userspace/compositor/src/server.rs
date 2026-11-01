//! Request dispatch — the server's decision-making, with no IPC in it.
//!
//! A pure function of (connection, request, [`WindowStack`]): the IPC loop, the framebuffer and
//! mapping the client's transferred `MemoryObject`s are the bin's job.
//!
//! Window ids are scoped to the connection that created them
//! (`docs/spec/rsproto-surface-ops.md`): naming another connection's window is `NotFound`, as if
//! it did not exist. [`WindowStack`] is compositor-wide so a shell can address any window, so
//! ownership is tracked here, per connection, and checked before dispatch reaches the stack;
//! otherwise the authority to create windows would be the authority to destroy anyone else's.

use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Relaxed;

use libkern::abi::HandleInfo;
use libkern::handle::KObjectType;
use librsproto::surface::{
    ConfigureEvent, OP_ATTACH_BUFFER, OP_COMMIT, OP_CREATE_WINDOW, OP_DESTROY_WINDOW, OP_SET_TITLE,
    build_create_window_reply, parse_attach_buffer_request, parse_commit_request, parse_create_window_request,
    parse_destroy_window_request,
};

use libdraw::geom::Rect;

use crate::{MAX_WINDOWS_PER_CONNECTION, StackError, WindowStack, union};

/// A client connection: the identity a request arrives on, and the windows it owns.
#[derive(Clone, Debug, Default)]
pub struct Connection {
    /// Windows created over this connection, in creation order.
    owned: Vec<u32>,
    /// Rejections logged so far; per connection so one noisy client cannot spend the whole
    /// machine's diagnostic budget.
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

    /// Drop `window` from what this connection claims. Silent if it never held it.
    ///
    /// For removals this connection did not ask for (a manager's `Manage::Close`); without it the
    /// client's own `DestroyWindow` would answer `InvalidArgument` where the rest answer
    /// `NotFound`.
    pub fn disown(&mut self, window: u32) {
        self.owned.retain(|id| *id != window);
    }
}

/// What a dispatched request produced.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A reply body of this length was written.
    Reply(usize),
    /// A window was created: reply with `reply_len` bytes, **then** send this `Configure`.
    ///
    /// The order is the contract: a client in `Window::new` reads the id and then waits for the
    /// configure, so the other way round would deadlock every client at startup. The configure
    /// is what makes the window compositable ([`WindowStack::mark_configured`]).
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
        /// `None` means "everything", the safe answer for anything that cannot name its region;
        /// `Some(empty)` means nothing changed on screen (attaching a buffer, say).
        dirty: Option<Rect>,
    },
    /// A window was renamed; the manager needs telling. A title changes nothing on screen, so this
    /// is not an [`Applied`](Outcome::Applied).
    TitleSet {
        /// The renamed window.
        window: u32,
    },
    /// The request was rejected.
    Failed(SurfaceError),
}

/// Why a request was refused.
///
/// Another connection's window is [`SurfaceError::NotFound`], so replies cannot probe for ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurfaceError {
    /// Undecodable body for this op.
    Malformed,
    /// The op is not one this server implements.
    Unsupported,
    /// No such window *for this connection*, including another connection's.
    NotFound,
    /// The request was well-formed but could not be applied.
    Rejected(StackError),
}

/// Whether an over-long title has already been reported.
static TITLE_TRUNCATED_LOGGED: AtomicBool = AtomicBool::new(false);

/// Whether this is the first over-long title.
fn first_truncation() -> bool {
    !TITLE_TRUNCATED_LOGGED.swap(true, Relaxed)
}

/// Report the **first** over-long title only: a serial write per request is too costly on this
/// path, and one line explains a title that came back shorter than it went out.
fn note_title_truncated(len: usize) {
    if !first_truncation() {
        return;
    }
    let mut l = libkern::debug::Line::new();
    l.s(b"compositor: title of ")
        .u(len as u64)
        .s(b" bytes truncated to ")
        .u(librsproto::surface::MAX_TITLE as u64)
        .s(b" (further truncations not logged)");
    l.end();
}

/// One client buffer's memory: a mapping in the binary, plain bytes in a host test.
pub struct Attached<M> {
    /// The window it was attached to.
    pub window: u32,
    /// The client's id for it.
    pub buffer: u32,
    /// What backs it.
    pub memory: M,
}

/// Whether a handle `info` describes is a `MemoryObject` of at least `len` bytes. A file maps too,
/// faulted in through its server on this thread, and a fill that fails there ends the compositor.
pub fn holds(info: &HandleInfo, len: usize) -> bool {
    info.object_type == KObjectType::MemoryObject as u32 && info.size >= len as u64
}

/// The memory an `AttachBuffer` in `body` brought, whose handle `info` describes, mapped by `map`.
/// `None` is a refusal, the client's fault. A `None` memory is a failed map, this compositor's
/// shortage and no fault of a client that cannot hear a refusal: the attach stands, and draws
/// nothing.
pub fn attach_memory<M>(
    body: &[u8],
    info: Option<&HandleInfo>,
    map: impl FnOnce(usize) -> Option<M>,
) -> Option<Attached<Option<M>>> {
    let req = parse_attach_buffer_request(body)?;
    let len = req.pitch as usize * req.height as usize;
    info.filter(|i| holds(i, len))?;
    Some(Attached { window: req.window, buffer: req.buffer, memory: map(len) })
}

/// The answer to an attach [`attach_memory`] refused: what `dispatch` says of a window this
/// connection does not own comes first, as it does for every op.
pub fn refuse_attach(conn: &Connection, body: &[u8]) -> Outcome {
    match parse_attach_buffer_request(body) {
        Some(req) if !conn.owns(req.window) => Outcome::Failed(SurfaceError::NotFound),
        _ => Outcome::Failed(SurfaceError::Malformed),
    }
}

/// Keep an attach's memory if `outcome` accepted it, under ids `dispatch` checked; a rejected one
/// drops here. The memory it replaces goes either way it came: a failed map must leave no pixels,
/// not the wrong ones.
pub fn record_attach<M>(buffers: &mut Vec<Attached<M>>, outcome: &Outcome, attached: Attached<Option<M>>) {
    if !matches!(outcome, Outcome::Applied { .. }) {
        return;
    }
    let Attached { window, buffer, memory } = attached;
    buffers.retain(|b| !(b.window == window && b.buffer == buffer));
    if let Some(memory) = memory {
        buffers.push(Attached { window, buffer, memory });
    }
}

/// Everything a removed window leaves, whoever removed it: its owner's claim, as a manager's
/// `Manage::Close` or a transitive destroy bypasses the client's inline prune
/// ([`Connection::disown`]), and its memory.
pub fn forget_window<M>(conns: &mut [Connection], buffers: &mut Vec<Attached<M>>, window: u32) {
    for conn in conns.iter_mut() {
        conn.disown(window);
    }
    buffers.retain(|b| b.window != window);
}

/// Dispatch one Surface-category request arriving on `conn`.
///
/// `body` excludes the envelope; a reply body, when there is one, goes into `reply`.
pub fn dispatch(conn: &mut Connection, stack: &mut WindowStack, op: u16, body: &[u8], reply: &mut [u8]) -> Outcome {
    match op {
        OP_CREATE_WINDOW => {
            let Some(req) = parse_create_window_request(body) else {
                return Outcome::Failed(SurfaceError::Malformed);
            };
            // Bounded: a client may hold several windows (a menu is a second one), and `conn.owned`
            // and the stack are plain `Vec`s.
            if conn.owned.len() >= MAX_WINDOWS_PER_CONNECTION {
                return Outcome::Failed(SurfaceError::Rejected(StackError::TooManyWindows));
            }
            // A parent must be one of *this* connection's windows, or a popup could be parented
            // onto a stranger's window and learn that the id exists.
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
                            // The default answer: the client's own request echoed back. With a
                            // manager attached the bin holds this and releases it with the geometry
                            // as it stands once the manager has acted.
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
            // Read before the commit: `bounds` reports the committed buffer, so a shrunk window
            // would clip its damage to the new rectangle and keep old pixels in the vacated band.
            // Painted bounds and whether it was drawn matter too: a shadow lies outside `bounds`,
            // so the first commit must repaint it.
            let was = stack.window(req.window).map(|w| (w.bounds(), w.painted_bounds()));
            let was_drawn = stack.window(req.window).is_some_and(|w| w.committed.is_some());
            match stack.commit(&req) {
                Ok(previous) => {
                    // The damage the client sent, in screen coordinates, clipped to the window: the
                    // clip bounds the work a commit can cost, not a leak (compositing clips every
                    // surface to its own bounds).
                    let dirty = stack.window(req.window).map(|w| {
                        let local = Rect::new(req.damage_x as i32, req.damage_y as i32, req.damage_w, req.damage_h);
                        let bounds = w.bounds();
                        // A window nothing has drawn yet repaints all of itself, shadow included,
                        // whatever damage it named.
                        if !was_drawn {
                            return w.painted_bounds();
                        }
                        // A window that changed shape repaints both shapes: the vacated band cannot
                        // be in the client's damage, and a rectangle cannot say "old minus new".
                        // Painted bounds, for the shadow.
                        if let Some((_, before)) = was.filter(|(b, _)| *b != bounds) {
                            return union(before, w.painted_bounds());
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
            // Same ownership rule as every other op: another connection's window is `NotFound`.
            if !conn.owns(id) {
                return Outcome::Failed(SurfaceError::NotFound);
            }
            let Some(w) = stack.window_mut(id) else {
                return Outcome::Failed(SurfaceError::NotFound);
            };
            let kept = librsproto::surface::title::truncate_title(title);
            if kept.len() < title.len() {
                note_title_truncated(title.len());
            }
            // An unchanged title is not manager traffic: the outbox is bounded and drops its
            // oldest.
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
            let before: Vec<(u32, Rect)> = stack.windows().iter().map(|w| (w.id, w.painted_bounds())).collect();
            match stack.destroy(window) {
                Ok(()) => {
                    // Destroy is transitive: drop every id this connection no longer holds, or a
                    // later id reuse could hand it someone else's window.
                    conn.owned.retain(|id| stack.window(*id).is_some());
                    // The union of every window that vanished: a popup's children go with it.
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
/// Destroy is transitive, so a parent takes its popups with it and the remaining ids resolve
/// to nothing.
pub fn disconnect(conn: &mut Connection, stack: &mut WindowStack) {
    for id in core::mem::take(&mut conn.owned) {
        // Already gone if an ancestor took it; that is not an error.
        let _ = stack.destroy(id);
    }
}

fn parent_of(role: &librsproto::surface::Role) -> Option<u32> {
    match role {
        librsproto::surface::Role::Popup { parent } | librsproto::surface::Role::Dialog { parent } => Some(*parent),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native runs can catch a lost `swap` only if the threads collide, so they collide many times.
    #[test]
    fn at_most_one_thread_reports_a_title_truncated() {
        use core::sync::atomic::AtomicU32;
        use std::sync::Barrier;
        let rounds = if cfg!(miri) { 2 } else { 20_000 };
        let (start, done, firsts) = (Barrier::new(3), Barrier::new(3), AtomicU32::new(0));
        let race = || {
            for _ in 0..rounds {
                start.wait();
                firsts.fetch_add(first_truncation() as u32, Relaxed);
                done.wait();
            }
        };
        let most = std::thread::scope(|s| {
            s.spawn(race);
            s.spawn(race);
            (0..rounds).fold(0, |most, _| {
                TITLE_TRUNCATED_LOGGED.store(false, Relaxed);
                firsts.store(0, Relaxed);
                start.wait();
                done.wait();
                most.max(firsts.load(Relaxed))
            })
        });
        assert!(most <= 1, "two threads each saw themselves first");
    }

    fn info(object: KObjectType, size: u64) -> HandleInfo {
        HandleInfo { rights: 0, object_type: object as u32, generation: 0, size }
    }

    fn attach_body(window: u32, buffer: u32) -> ([u8; ATTACH_BUFFER_REQUEST_LEN], usize) {
        let mut body = [0u8; ATTACH_BUFFER_REQUEST_LEN];
        let req =
            AttachBufferRequest { window, buffer, width: 8, height: 8, pitch: 32, format: SURFACE_FORMAT_XRGB8888 };
        let n = build_attach_buffer_request(&mut body, &req).unwrap();
        (body, n)
    }

    /// `8 * 8 * 4`, the geometry [`attach_body`] asks for.
    const ATTACH_LEN: u64 = 256;

    #[test]
    fn only_memory_that_holds_the_geometry_is_mapped() {
        let (body, n) = attach_body(1, 0);
        let mapped = |i: Option<&HandleInfo>| attach_memory(&body[..n], i, Some).map(|a| a.memory);
        assert_eq!(mapped(Some(&info(KObjectType::MemoryObject, ATTACH_LEN))), Some(Some(ATTACH_LEN as usize)));
        assert_eq!(mapped(Some(&info(KObjectType::MemoryObject, ATTACH_LEN - 1))), None, "short of the geometry");
        assert_eq!(mapped(Some(&info(KObjectType::FileObject, ATTACH_LEN))), None, "a file is not memory");
        assert_eq!(mapped(None), None, "no handle, or one that cannot say what it is");
    }

    #[test]
    fn a_map_that_fails_is_no_refusal() {
        let (body, n) = attach_body(1, 0);
        let good = info(KObjectType::MemoryObject, ATTACH_LEN);
        let a = attach_memory::<u8>(&body[..n], Some(&good), |_| None).expect("the client did nothing wrong");
        assert!(a.memory.is_none());
    }

    #[test]
    fn a_refused_attach_on_a_foreign_window_is_not_found() {
        let (mut stack, mut mine, other) = (WindowStack::new(), Connection::new(), Connection::new());
        let w = create(&mut mine, &mut stack, Role::Normal).unwrap();
        let (body, n) = attach_body(w, 0);
        assert!(matches!(refuse_attach(&mine, &body[..n]), Outcome::Failed(SurfaceError::Malformed)));
        assert!(matches!(refuse_attach(&other, &body[..n]), Outcome::Failed(SurfaceError::NotFound)));
    }

    fn table() -> Vec<Attached<u8>> {
        alloc::vec![Attached { window: 1, buffer: 0, memory: 10 }, Attached { window: 2, buffer: 0, memory: 20 }]
    }

    fn memory_of(buffers: &[Attached<u8>]) -> Vec<(u32, u8)> {
        buffers.iter().map(|b| (b.window, b.memory)).collect()
    }

    const APPLIED: Outcome = Outcome::Applied { release: None, dirty: None };

    #[test]
    fn a_rejected_attach_records_nothing_and_takes_nothing() {
        let mut t = table();
        let refused = Outcome::Failed(SurfaceError::NotFound);
        record_attach(&mut t, &refused, Attached { window: 2, buffer: 0, memory: Some(99) });
        assert_eq!(memory_of(&t), [(1, 10), (2, 20)], "a foreign window's memory is not replaced");
    }

    #[test]
    fn an_applied_attach_replaces_its_buffer() {
        let mut t = table();
        record_attach(&mut t, &APPLIED, Attached { window: 2, buffer: 0, memory: Some(99) });
        assert_eq!(memory_of(&t), [(1, 10), (2, 99)]);
    }

    #[test]
    fn a_failed_map_leaves_no_pixels_rather_than_the_wrong_ones() {
        let mut t = table();
        record_attach(&mut t, &APPLIED, Attached { window: 2, buffer: 0, memory: None });
        assert_eq!(memory_of(&t), [(1, 10)]);
    }

    #[test]
    fn a_window_that_goes_takes_its_memory_and_its_owner_s_claim() {
        let (mut stack, mut conns) = (WindowStack::new(), [Connection::new(), Connection::new()]);
        let w = create(&mut conns[0], &mut stack, Role::Normal).unwrap();
        let mut t = alloc::vec![
            Attached { window: w, buffer: 0, memory: 10 },
            Attached { window: w + 1, buffer: 0, memory: 20 }
        ];
        forget_window(&mut conns, &mut t, w);
        assert!(!conns[0].owns(w));
        assert_eq!(memory_of(&t), [(w + 1, 20)]);
    }

    #[track_caller]
    fn assert_applied(o: Outcome, release: Option<(u32, u32)>) {
        match o {
            Outcome::Applied { release: r, .. } => assert_eq!(r, release, "release"),
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[track_caller]
    fn dirty_of(o: Outcome) -> Option<Rect> {
        match o {
            Outcome::Applied { dirty, .. } => dirty,
            other => panic!("expected Applied, got {other:?}"),
        }
    }
    use librsproto::surface::{
        ATTACH_BUFFER_REQUEST_LEN, AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge, Role,
        SURFACE_FORMAT_XRGB8888, build_attach_buffer_request, build_commit_request, build_create_window_request,
        build_destroy_window_request, parse_create_window_reply,
    };

    pub(super) fn create(conn: &mut Connection, stack: &mut WindowStack, role: Role) -> Result<u32, SurfaceError> {
        create_sized(conn, stack, role, 8, 8)
    }

    fn create_sized(
        conn: &mut Connection,
        stack: &mut WindowStack,
        role: Role,
        width: u32,
        height: u32,
    ) -> Result<u32, SurfaceError> {
        let mut body = [0u8; 32];
        let n = build_create_window_request(&mut body, &CreateWindowRequest::new(width, height, role)).unwrap();
        let mut reply = [0u8; 32];
        match dispatch(conn, stack, OP_CREATE_WINDOW, &body[..n], &mut reply) {
            Outcome::Created { reply_len, .. } => {
                let id = parse_create_window_reply(&reply[..reply_len]).unwrap();
                // Stands in for the bin, which marks a window configured when it sends the first
                // `Configure`.
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
            &AttachBufferRequest { window, buffer, width, height, pitch: width * 4, format: SURFACE_FORMAT_XRGB8888 },
        )
        .unwrap();
        let mut reply = [0u8; 8];
        dispatch(conn, stack, OP_ATTACH_BUFFER, &body[..n], &mut reply)
    }

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
            &CommitRequest { window, buffer, damage_x: d.0, damage_y: d.1, damage_w: d.2, damage_h: d.3 },
        )
        .unwrap();
        let mut reply = [0u8; 8];
        dispatch(conn, stack, OP_COMMIT, &body[..n], &mut reply)
    }

    fn commit(conn: &mut Connection, stack: &mut WindowStack, window: u32, buffer: u32) -> Outcome {
        let mut body = [0u8; 32];
        let n = build_commit_request(
            &mut body,
            &CommitRequest { window, buffer, damage_x: 0, damage_y: 0, damage_w: 8, damage_h: 8 },
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
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, w, 0), None);
        // The first commit repaints the whole window, shadow included: the window appearing.
        let first = dirty_of(commit_damage(&mut a, &mut stack, w, 0, (2, 3, 4, 5)));
        let painted = stack.window(w).expect("in the stack").painted_bounds();
        assert_eq!(first, Some(painted), "a window's first frame paints its shadow too");
        assert!(painted.origin.x < 0, "the premise: painted bounds reach outside the window");

        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 0, (2, 3, 4, 5)));
        assert_eq!(d, Some(Rect::new(2, 3, 4, 5)));
    }

    #[test]
    fn a_commits_damage_is_clipped_to_its_own_window() {
        // A client is not trusted to bound its own rectangle.
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, w, 0), None);
        assert_applied(commit(&mut a, &mut stack, w, 0), None);
        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 0, (0, 0, 400, 400)));
        assert_eq!(d, Some(Rect::new(0, 0, 8, 8)));
        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 0, (100, 100, 4, 4)));
        assert_eq!(d.map(|r| (r.size.w, r.size.h)), Some((0, 0)));
    }

    #[test]
    fn attaching_a_buffer_dirties_nothing() {
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        let d = dirty_of(attach(&mut a, &mut stack, w, 0)).expect("attach names a region");
        assert!(d.size.w == 0 || d.size.h == 0, "attach dirtied {d:?}");
    }

    #[test]
    fn destroying_dirties_what_vanished_including_a_child_bigger_than_its_parent() {
        // The child is 16×16 to the parent's 8×8: a child that is a subset of its parent cannot
        // tell the union from the named window alone.
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

        // Painted bounds, captured before the destroy.
        let want = union(
            stack.window(parent).expect("still here").painted_bounds(),
            stack.window(popup).expect("still here").painted_bounds(),
        );
        let d = dirty_of(destroy(&mut a, &mut stack, parent)).expect("destroy names a region");
        assert!(stack.window(popup).is_none(), "the premise: destroy took the child too");
        assert_eq!(d, want, "the union of both, not the parent's 8×8");
        assert!(want.size.w > 16, "the premise: a destroyed window erases its shadow as well");
    }

    #[test]
    fn a_window_whose_buffer_shrinks_repaints_what_it_vacated() {
        // `Window::bounds` reports the committed buffer, so a shrunk window must repaint the band
        // it vacated.
        let mut a = Connection::new();
        let mut stack = WindowStack::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_applied(attach(&mut a, &mut stack, w, 0), None);
        assert_applied(commit(&mut a, &mut stack, w, 0), None);

        assert_applied(attach_sized(&mut a, &mut stack, w, 1, 4, 4), None);
        let before = stack.window(w).expect("in the stack").painted_bounds();
        let d = dirty_of(commit_damage(&mut a, &mut stack, w, 1, (0, 0, 4, 4))).expect("a commit names a region");
        let after = stack.window(w).expect("in the stack").painted_bounds();
        // The union of what it painted before and now (painted bounds).
        assert_eq!(d, union(before, after), "only the new 4x4 was repainted, vacating a band");
        assert!(d.size.w > 8, "the premise: the vacated band and the shadow are both in it");
    }

    #[test]
    fn another_connection_s_window_is_not_found_not_denied() {
        // The two answers must be indistinguishable, or replies probe for which ids exist.
        let mut stack = WindowStack::new();
        let (mut a, mut b) = (Connection::new(), Connection::new());
        let real = create(&mut a, &mut stack, Role::Normal).unwrap();
        let never_existed = 9999;
        assert_eq!(destroy(&mut b, &mut stack, real), destroy(&mut b, &mut stack, never_existed));
    }

    /// A connection cannot hold more than [`MAX_WINDOWS_PER_CONNECTION`] windows at once.
    #[test]
    fn a_connection_is_bounded_in_how_many_windows_it_may_hold() {
        let mut stack = WindowStack::new();
        let mut conn = Connection::new();
        for i in 0..MAX_WINDOWS_PER_CONNECTION {
            create(&mut conn, &mut stack, Role::Normal).unwrap_or_else(|e| panic!("window {i} refused: {e:?}"));
        }
        assert_eq!(
            create(&mut conn, &mut stack, Role::Normal),
            Err(SurfaceError::Rejected(StackError::TooManyWindows)),
            "the one past the bound is refused, and says why"
        );

        // Destroying one makes room again: the bound is on what is held.
        let first = conn.owned()[0];
        assert!(matches!(destroy(&mut conn, &mut stack, first), Outcome::Applied { .. }));
        create(&mut conn, &mut stack, Role::Normal).expect("room again");

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
        // A create answers `Created`, not `Applied`, yet moves focus; `serve_session` must announce
        // focus after every request. This pins the premise only; the boot gate's check that a
        // window's first event is its focus change is what catches the regression.
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let first = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_eq!(stack.focus_candidate(), Some(first));

        let mut body = [0u8; 32];
        let n = build_create_window_request(&mut body, &CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        let mut reply = [0u8; 32];
        let outcome = dispatch(&mut a, &mut stack, OP_CREATE_WINDOW, &body[..n], &mut reply);
        assert!(
            !matches!(outcome, Outcome::Applied { .. }),
            "a create is not an Applied, so anything keyed on Applied misses it: {outcome:?}"
        );
        let Outcome::Created { reply_len, configure } = outcome else { panic!("unexpected {outcome:?}") };
        let second = parse_create_window_reply(&reply[..reply_len]).unwrap();
        assert_eq!(configure.window, second, "the configure names the window just created");

        assert_ne!(second, first);

        // Focus follows the first `Configure`, not the create: a held window is not on screen and
        // must not hold the keyboard.
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
        assert_applied(commit(&mut a, &mut stack, w, 1), Some((w, 0)));
    }

    #[test]
    fn destroying_a_parent_drops_the_descendants_from_the_connection_too() {
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
    fn a_window_removed_by_someone_else_stops_being_owned() {
        // A manager's `Manage::Close` removes a window its owner still believes in; the removal
        // drain calls `Connection::disown`. Without it the client's own `DestroyWindow` would
        // answer `InvalidArgument` where every other request answers `NotFound`.
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        let menu = create(&mut a, &mut stack, Role::Popup { parent: w }).unwrap();

        stack.destroy(w).unwrap();
        for gone in stack.take_removed() {
            a.disown(gone);
        }

        assert!(a.owned().is_empty(), "still claiming: {:?}", a.owned());
        for gone in [w, menu] {
            assert_eq!(
                destroy(&mut a, &mut stack, gone),
                Outcome::Failed(SurfaceError::NotFound),
                "the answer every other request gives for a window that is not there"
            );
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
        assert_eq!(dispatch(&mut a, &mut stack, 0x09FE, &[], &mut reply), Outcome::Failed(SurfaceError::Unsupported));
    }

    #[test]
    fn a_panel_can_be_created_over_a_connection_like_any_other_window() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let p = create(&mut a, &mut stack, Role::Panel { dock: Edge::Top, reserve: 24 }).unwrap();
        assert!(a.owns(p));
        assert_eq!(stack.work_area(libdraw::geom::Rect::new(0, 0, 100, 50)), libdraw::geom::Rect::new(0, 24, 100, 26));
    }
}

#[cfg(test)]
mod title_tests {
    use super::tests::create;
    use super::*;
    use crate::Role;
    use librsproto::surface::{MAX_TITLE, title};

    fn set_title(conn: &mut Connection, stack: &mut WindowStack, window: u32, s: &str) -> Outcome {
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
        assert!(matches!(set_title(&mut a, &mut stack, w, "other"), Outcome::TitleSet { .. }));
    }

    #[test]
    fn another_connections_window_cannot_be_renamed() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let mut b = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert!(matches!(set_title(&mut b, &mut stack, w, "mine now"), Outcome::Failed(SurfaceError::NotFound)));
        assert_eq!(stack.window(w).unwrap().title, "", "the window was renamed anyway");
        assert!(matches!(set_title(&mut b, &mut stack, w + 99, "ghost"), Outcome::Failed(SurfaceError::NotFound)));
    }

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
