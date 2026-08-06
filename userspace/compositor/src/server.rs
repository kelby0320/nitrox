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
    OP_ATTACH_BUFFER, OP_COMMIT, OP_CREATE_WINDOW, OP_DESTROY_WINDOW, build_create_window_reply,
    parse_attach_buffer_request, parse_commit_request, parse_create_window_request,
    parse_destroy_window_request,
};

use crate::{StackError, WindowStack};

/// A client connection: the identity a request arrives on, and the windows it owns.
#[derive(Clone, Debug, Default)]
pub struct Connection {
    /// Windows created over this connection, in creation order.
    owned: Vec<u32>,
}

impl Connection {
    /// A connection owning nothing.
    pub fn new() -> Self {
        Self { owned: Vec::new() }
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
    /// Applied with no reply body.
    Applied {
        /// The `(window, buffer)` to release to the client, if any — **the buffer that
        /// left the screen**, never the one that arrived.
        release: Option<(u32, u32)>,
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
                        Some(n) => Outcome::Reply(n),
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
                Ok(()) => Outcome::Applied { release: None },
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
            match stack.commit(&req) {
                Ok(previous) => {
                    Outcome::Applied { release: previous.map(|b| (req.window, b)) }
                }
                Err(e) => Outcome::Failed(SurfaceError::Rejected(e)),
            }
        }

        OP_DESTROY_WINDOW => {
            let Some(window) = parse_destroy_window_request(body) else {
                return Outcome::Failed(SurfaceError::Malformed);
            };
            if !conn.owns(window) {
                return Outcome::Failed(SurfaceError::NotFound);
            }
            match stack.destroy(window) {
                Ok(()) => {
                    // Destroy is transitive, so this connection's descendants of `window`
                    // are gone from the stack too. Drop every id it no longer holds, or
                    // the connection keeps claiming ownership of windows that do not
                    // exist — and a later id reuse would hand it someone else's window.
                    conn.owned.retain(|id| stack.window(*id).is_some());
                    Outcome::Applied { release: None }
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
    use librsproto::surface::{
        ATTACH_BUFFER_REQUEST_LEN, AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge,
        Role, SURFACE_FORMAT_XRGB8888, build_attach_buffer_request, build_commit_request,
        build_create_window_request, build_destroy_window_request, parse_create_window_reply,
    };

    fn create(conn: &mut Connection, stack: &mut WindowStack, role: Role) -> Result<u32, SurfaceError> {
        let mut body = [0u8; 32];
        let n = build_create_window_request(
            &mut body,
            &CreateWindowRequest { width: 8, height: 8, role },
        )
        .unwrap();
        let mut reply = [0u8; 32];
        match dispatch(conn, stack, OP_CREATE_WINDOW, &body[..n], &mut reply) {
            Outcome::Reply(len) => Ok(parse_create_window_reply(&reply[..len]).unwrap()),
            Outcome::Failed(e) => Err(e),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn attach(conn: &mut Connection, stack: &mut WindowStack, window: u32, buffer: u32) -> Outcome {
        let mut body = [0u8; ATTACH_BUFFER_REQUEST_LEN];
        let n = build_attach_buffer_request(
            &mut body,
            &AttachBufferRequest {
                window,
                buffer,
                width: 8,
                height: 8,
                pitch: 32,
                format: SURFACE_FORMAT_XRGB8888,
            },
        )
        .unwrap();
        let mut reply = [0u8; 8];
        dispatch(conn, stack, OP_ATTACH_BUFFER, &body[..n], &mut reply)
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
    fn another_connection_s_window_is_not_found_not_denied() {
        // The two answers must be indistinguishable, or the reply is an oracle for probing
        // which window ids exist.
        let mut stack = WindowStack::new();
        let (mut a, mut b) = (Connection::new(), Connection::new());
        let real = create(&mut a, &mut stack, Role::Normal).unwrap();
        let never_existed = 9999;
        assert_eq!(destroy(&mut b, &mut stack, real), destroy(&mut b, &mut stack, never_existed));
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
    fn commit_reports_the_buffer_to_release() {
        let mut stack = WindowStack::new();
        let mut a = Connection::new();
        let w = create(&mut a, &mut stack, Role::Normal).unwrap();
        assert_eq!(attach(&mut a, &mut stack, w, 0), Outcome::Applied { release: None });
        assert_eq!(attach(&mut a, &mut stack, w, 1), Outcome::Applied { release: None });

        assert_eq!(commit(&mut a, &mut stack, w, 0), Outcome::Applied { release: None });
        assert_eq!(
            commit(&mut a, &mut stack, w, 1),
            Outcome::Applied { release: Some((w, 0)) },
            "the buffer that left the screen"
        );
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

        assert_eq!(destroy(&mut a, &mut stack, w), Outcome::Applied { release: None });
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
