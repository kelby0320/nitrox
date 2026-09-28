//! `Services` (`op = 0x11xx`) — starting, stopping and restarting services, served by `service-mgr`
//! on an admin session. See `docs/spec/rsproto-services-ops.md` and
//! `docs/planning/administration.md` § Part E.
//!
//! **An admin session is a channel `service-mgr` answers any resolve on its admin endpoint with.**
//! The endpoint is minted at `/svc/services/admin-endpoint`, and the view broker binds it at
//! `/dev/services/admin` in a view with the `services` grant. What reaches it is therefore the
//! grant's to decide; `service-mgr` answers every request on it, and refuses an essential service's
//! stop or restart itself.
//!
//! **The list is not an op**: it is a table, `all.tsm`, resolved from `/svc/services` or a
//! session's `/dev/services`, as the device manager's and the storage service's are.
//!
//! Each request's body is the service's name, as its declaration spells it. The reply's body is
//! empty. A refusal is the standard `ErrorBody`, and its reason says which step refused.

/// Client → `service-mgr`: **start a service that is not running**. Answered once it is up — a
/// server when its `Meta::Ready` has been bound — or refused.
pub const OP_SERVICES_START: u16 = 0x1100;
/// Client → `service-mgr`: **stop a running service**, by `CTRL_OP_SHUTDOWN` on its control
/// channel. Answered once it has exited, or refused `TimedOut` if it is still running when the
/// bound runs out: a stop is a request, and there is no forcible kill. An `essential` service is
/// refused `NoAccess`.
pub const OP_SERVICES_STOP: u16 = 0x1101;
/// Client → `service-mgr`: **stop a service and start it again**, or start it if it was not
/// running. Answered once the new instance is up. An `essential` service is refused `NoAccess`.
pub const OP_SERVICES_RESTART: u16 = 0x1102;
