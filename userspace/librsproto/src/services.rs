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
//!
//! **A power session** (administration Part E.4b) is the same shape on a different endpoint:
//! `power-endpoint`, minted from `/svc/services` only, and bound by the view broker's `power` grant
//! at `/dev/power`. It takes [`OP_SERVICES_SHUTDOWN`] and nothing else, and an admin session does
//! not take that.

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
/// Client → `service-mgr`, on a **power session** only: **shut the machine down**, or reboot it.
/// The body is one byte, [`SHUTDOWN_HALT`] or [`SHUTDOWN_REBOOT`]. Answered as soon as the
/// shutdown has begun — nothing comes back after it — or refused `WouldBlock` while one is already
/// under way.
pub const OP_SERVICES_SHUTDOWN: u16 = 0x1103;
/// `Shutdown`'s body: halt, and say it is safe to turn the machine off.
pub const SHUTDOWN_HALT: u8 = 0;
/// `Shutdown`'s body: reset the machine.
pub const SHUTDOWN_REBOOT: u8 = 1;

/// `Shutdown`'s body for `reboot`.
pub fn shutdown_body(reboot: bool) -> [u8; 1] {
    [if reboot { SHUTDOWN_REBOOT } else { SHUTDOWN_HALT }]
}

/// Parse `Shutdown`'s body: whether to reboot, or `None` for anything but one byte that names a
/// shutdown.
pub fn parse_shutdown(body: &[u8]) -> Option<bool> {
    match body {
        [SHUTDOWN_HALT] => Some(false),
        [SHUTDOWN_REBOOT] => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A body is one byte naming a shutdown**; what no correct client sends is refused — an
    /// empty body, a longer one, and a byte that names nothing.
    #[test]
    fn a_shutdown_body_round_trips_and_nothing_else_parses() {
        assert_eq!(parse_shutdown(&shutdown_body(false)), Some(false));
        assert_eq!(parse_shutdown(&shutdown_body(true)), Some(true));
        assert_eq!(parse_shutdown(&[]), None);
        assert_eq!(parse_shutdown(&[SHUTDOWN_REBOOT, 0]), None);
        assert_eq!(parse_shutdown(&[2]), None);
    }
}
