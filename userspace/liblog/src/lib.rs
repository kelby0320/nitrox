//! `liblog` — the client side of the logging service.

#![cfg_attr(not(test), no_std)]

use libkern::{RawHandle, Rights};
use libos::{CanLookup, Carried, Channel, Handle, Msg, Namespace, NsReadOnly, Only, Ref, block_on};
use librsproto::log::encode_append;

pub use librsproto::log::{LEVEL_CRITICAL, LEVEL_DEBUG, LEVEL_ERROR, LEVEL_INFO, LEVEL_TRACE, LEVEL_WARN};

/// Where a logger's records go.
enum Sink {
    /// Nowhere: every emit is a no-op.
    None,
    /// An endpoint the logger owns, closed with it.
    Owned(Handle<Channel, Only>),
    /// An endpoint its caller owns and keeps open while the logger lives.
    Borrowed(u64),
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

    /// Log through `handle`, which the caller keeps owning: the logger closes nothing. For callers
    /// not yet on libos; the typed form is [`new`](Self::new).
    // TODO(raw-handle-shims): goes when its last caller is on libos.
    pub const fn borrowing(handle: u64) -> Self {
        Self { sink: if handle == 0 { Sink::None } else { Sink::Borrowed(handle) } }
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
        let _ = match &self.sink {
            Sink::None => return,
            Sink::Owned(endpoint) => endpoint.send(&msg, &mut Carried::new()),
            Sink::Borrowed(raw) => {
                // SAFETY: `borrowing`'s caller keeps the endpoint open while the logger lives.
                let endpoint = unsafe { Ref::<Channel, Only>::from_raw(RawHandle(*raw), Rights::empty()) };
                endpoint.send(&msg, &mut Carried::new())
            }
        };
    }
}

/// Resolve a log path in `ns` to a [`Logger`] that owns its endpoint, or to an invalid one. For
/// callers not yet on libos; the typed form is [`Logger::open`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn open_source(ns: u64, path: &[u8]) -> Logger {
    // SAFETY: the caller's own namespace handle, open for the call, as this has always asked.
    let ns = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(ns), Rights::empty()) };
    match core::str::from_utf8(path) {
        Ok(path) => Logger::open(ns, path),
        Err(_) => Logger::none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libos::mock;
    use librsproto::log::parse_append;

    /// A namespace whose `/log/x` answers with an endpoint, and its peer, where records arrive.
    fn service() -> (u64, Ref<'static, Channel, Only>) {
        let (ns, peer) = mock::answering("/log/x");
        // SAFETY: the mock's handle, open for the test's life.
        (ns, unsafe { Ref::from_raw(RawHandle(peer), Rights::empty()) })
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
        let log = open_source(ns, b"/log/x");
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
        let log = open_source(ns, b"/log/y");
        assert!(!log.is_valid());
        log.info("lost");
        assert!(!Logger::none().is_valid() && !Logger::borrowing(0).is_valid());
    }

    #[test]
    fn a_record_past_a_message_is_dropped_whole() {
        mock::reset();
        let (ns, peer) = service();
        let log = open_source(ns, b"/log/x");
        let long = "x".repeat(libkern::IPC_PAYLOAD_SIZE);
        log.info(&long);
        log.info("after");
        assert_eq!(received(peer), Some((LEVEL_INFO, b"after".to_vec())), "nothing sent for the long one");
    }

    #[test]
    fn a_borrowing_logger_sends_and_closes_nothing() {
        mock::reset();
        let (ns, peer) = service();
        // SAFETY: the mock's handle, open for the test.
        let root = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(ns), Rights::empty()) };
        let endpoint: Handle<Channel, Only> = block_on(root.lookup("/log/x", Rights::SEND)).unwrap();
        let log = Logger::borrowing(endpoint.raw().0);
        log.warn("borrowed");
        drop(log);
        assert!(mock::is_open(endpoint.raw().0), "the caller's endpoint stays open");
        assert_eq!(received(peer), Some((LEVEL_WARN, b"borrowed".to_vec())));
    }
}
