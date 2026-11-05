//! `liblog` — the client side of the logging service.

#![cfg_attr(not(test), no_std)]

use libkern::Rights;
use libos::{CanLookup, Carried, Channel, Handle, Msg, Namespace, Only, Ref, block_on};
use librsproto::log::encode_append;

pub use librsproto::log::{LEVEL_CRITICAL, LEVEL_DEBUG, LEVEL_ERROR, LEVEL_INFO, LEVEL_TRACE, LEVEL_WARN};

/// Where a logger's records go.
enum Sink {
    /// Nowhere: every emit is a no-op.
    None,
    /// An endpoint the logger owns, closed with it.
    Owned(Handle<Channel, Only>),
}

/// A log endpoint, or none; a record that does not fit a message, or finds the sink full, is
/// dropped rather than truncated or waited for.
pub struct Logger {
    sink: Sink,
}

impl Logger {
    /// Log through `endpoint`, which the logger owns and closes.
    pub fn new(endpoint: Handle<Channel, Only>) -> Self {
        Self { sink: Sink::Owned(endpoint) }
    }

    /// A logger that sends nothing.
    pub const fn none() -> Self {
        Self { sink: Sink::None }
    }

    /// Resolve the log path `path` in `ns` to a logger owning its endpoint, or to one that sends
    /// nothing if it does not resolve.
    pub fn open<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Self {
        match block_on(ns.into().lookup(path, Rights::SEND | Rights::TRANSFER)) {
            Ok(endpoint) => Self::new(endpoint),
            Err(_) => Self::none(),
        }
    }

    /// The endpoint, for a caller that sends records of its own on it, as a typed stream.
    pub fn endpoint(&self) -> Option<Ref<'_, Channel, Only>> {
        match &self.sink {
            Sink::None => None,
            Sink::Owned(endpoint) => Some(endpoint.as_ref()),
        }
    }

    /// Whether the endpoint is present.
    pub const fn is_valid(&self) -> bool {
        !matches!(self.sink, Sink::None)
    }

    /// Emit `message` at `level`.
    pub fn log(&self, level: u8, message: &str) {
        self.emit(level, message, None);
    }

    /// Emit `message` at `level` with a self-declared `source`.
    pub fn log_source(&self, level: u8, message: &str, source: &str) {
        self.emit(level, message, Some(source));
    }

    /// Emit `message` at trace level.
    pub fn trace(&self, message: &str) {
        self.log(LEVEL_TRACE, message);
    }
    /// Emit `message` at debug level.
    pub fn debug(&self, message: &str) {
        self.log(LEVEL_DEBUG, message);
    }
    /// Emit `message` at info level.
    pub fn info(&self, message: &str) {
        self.log(LEVEL_INFO, message);
    }
    /// Emit `message` at warn level.
    pub fn warn(&self, message: &str) {
        self.log(LEVEL_WARN, message);
    }
    /// Emit `message` at error level.
    pub fn error(&self, message: &str) {
        self.log(LEVEL_ERROR, message);
    }
    /// Emit `message` at critical level.
    pub fn critical(&self, message: &str) {
        self.log(LEVEL_CRITICAL, message);
    }

    fn emit(&self, level: u8, message: &str, source: Option<&str>) {
        if !self.is_valid() {
            return;
        }
        let mut msg = Msg::new();
        let Some(n) =
            encode_append(msg.payload_mut(), level, message.as_bytes(), None, None, source.map(str::as_bytes))
        else {
            return;
        };
        if msg.set_len(n).is_err() {
            return;
        }
        // A full or closed sink drops the record: logging never waits.
        if let Sink::Owned(endpoint) = &self.sink {
            let _ = endpoint.send(&msg, &mut Carried::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::RawHandle;
    use libos::{NsReadOnly, mock};
    use librsproto::log::parse_append;

    /// A namespace whose `/log/x` answers with an endpoint, and its peer, where records arrive.
    fn service() -> (Ref<'static, Namespace, NsReadOnly>, Ref<'static, Channel, Only>) {
        let (ns, peer) = mock::answering("/log/x");
        // SAFETY: the mock's handles, open for the test's life.
        unsafe { (Ref::from_raw(RawHandle(ns), Rights::LOOKUP), Ref::from_raw(RawHandle(peer), Rights::empty())) }
    }

    fn received(peer: Ref<'_, Channel, Only>) -> Option<(u8, std::vec::Vec<u8>)> {
        let mut msg = Msg::new();
        peer.recv(&mut msg).ok()?;
        parse_append(msg.payload()).map(|r| (r.level, r.message.to_vec()))
    }

    #[test]
    fn an_opened_logger_sends_each_record_and_owns_its_endpoint() {
        mock::reset();
        let (ns, peer) = service();
        let log = Logger::open(ns, "/log/x");
        assert!(log.is_valid());
        log.info("hello");
        log.error("bad");
        assert_eq!(received(peer), Some((LEVEL_INFO, b"hello".to_vec())));
        assert_eq!(received(peer), Some((LEVEL_ERROR, b"bad".to_vec())));
        let open = mock::open_handles();
        drop(log);
        assert_eq!(mock::open_handles(), open - 1, "the endpoint goes with the logger");
    }

    #[test]
    fn a_path_that_does_not_resolve_logs_nowhere() {
        mock::reset();
        let (ns, _peer) = service();
        let log = Logger::open(ns, "/log/y");
        assert!(!log.is_valid());
        log.info("lost");
        assert!(!Logger::none().is_valid() && Logger::none().endpoint().is_none());
    }

    #[test]
    fn a_record_past_a_message_is_dropped_whole() {
        mock::reset();
        let (ns, peer) = service();
        let log = Logger::open(ns, "/log/x");
        let long = "x".repeat(libkern::IPC_PAYLOAD_SIZE);
        log.info(&long);
        log.info("after");
        assert_eq!(received(peer), Some((LEVEL_INFO, b"after".to_vec())), "nothing sent for the long one");
    }

    #[test]
    fn a_logger_lends_its_endpoint_for_records_of_the_callers_own() {
        mock::reset();
        let (ns, peer) = service();
        let log = Logger::open(ns, "/log/x");
        let mut own = Msg::new();
        own.set(b"TSM1").unwrap();
        log.endpoint().unwrap().send(&own, &mut Carried::new()).unwrap();
        log.warn("after");
        let mut msg = Msg::new();
        peer.recv(&mut msg).unwrap();
        assert_eq!(msg.payload(), b"TSM1", "the caller's own, as sent");
        assert_eq!(received(peer), Some((LEVEL_WARN, b"after".to_vec())), "on the same endpoint");
    }
}
