//! Channel transport — carry a TSM1 byte stream across an IPC channel.
//!
//! A producer's [`ChannelSink`] frames the stream into messages over a [`MsgPort`]; a
//! consumer's [`ChannelReceiver`] reassembles them. Only [`IpcPort`] touches a syscall.
//!
//! ## Framing
//!
//! The stream is split into chunks of at most `max_chunk` bytes, each one message; the
//! **final** message carries a `last` marker (an `IpcMsgHeader.flags` bit). A stream always
//! ends with exactly one `last` message, even an empty one. Chunk boundaries are
//! byte-arbitrary: a record may straddle two messages.
//!
//! ## Backpressure & early close
//!
//! A [`MsgPort::send`] blocks while the channel's bounded queue is full, so a fast
//! producer paces itself to a slow consumer. If the consumer closes its read end, the
//! next `send` returns [`WireError::PeerClosed`]; a producer treats that as "stop
//! producing, exit cleanly" (`yes | head -1`).

use alloc::vec::Vec;

use crate::wire::{ByteSink, Result, WireError};

/// A message-oriented transport for one TSM1 stream: send/receive one framed message
/// at a time.
pub trait MsgPort {
    /// Send one message carrying `payload` (`≤ max_chunk` bytes); `last` marks the final
    /// message of the stream. Blocks while the channel queue is full (backpressure).
    /// Returns [`WireError::PeerClosed`] if the peer has closed its end.
    fn send(&mut self, payload: &[u8], last: bool) -> Result<()>;

    /// Receive the next message, appending its payload to `out` and returning whether it
    /// was the `last` message. Blocks until a message is available. Returns
    /// [`WireError::PeerClosed`] if the peer closed before sending a `last` message.
    fn recv(&mut self, out: &mut Vec<u8>) -> Result<bool>;
}

/// A [`ByteSink`] that frames its bytes into `≤ max_chunk` messages on a [`MsgPort`],
/// flushing full chunks eagerly. Call [`finish`](Self::finish) once the stream is complete
/// to emit the terminating `last` message.
///
/// Typically wrapped in a [`TableWriter`](crate::table::TableWriter):
/// ```ignore
/// let mut tw = TableWriter::new(ChannelSink::new(port, IPC_PAYLOAD_SIZE));
/// tw.write_schema(StreamFlags::NONE, &schema)?;
/// tw.write_row(&row)?;
/// tw.finish_with_status(0)?;      // writes the TSM1 terminator
/// tw.into_sink().finish()?;       // flushes the last channel message
/// ```
pub struct ChannelSink<P: MsgPort> {
    port: P,
    buf: Vec<u8>,
    max_chunk: usize,
    finished: bool,
}

impl<P: MsgPort> ChannelSink<P> {
    /// Wrap `port`, framing into messages of at most `max_chunk` payload bytes
    /// (`max_chunk` must be non-zero — the real port passes `IPC_PAYLOAD_SIZE`).
    pub fn new(port: P, max_chunk: usize) -> Self {
        debug_assert!(max_chunk > 0, "max_chunk must be non-zero");
        ChannelSink { port, buf: Vec::new(), max_chunk, finished: false }
    }

    fn flush_full_chunks(&mut self) -> Result<()> {
        while self.buf.len() >= self.max_chunk {
            self.port.send(&self.buf[..self.max_chunk], false)?;
            self.buf.drain(..self.max_chunk);
        }
        Ok(())
    }

    /// Send the buffered tail as the terminating `last` message, closing the stream.
    /// Emits exactly one `last` message (an empty one if the stream ended on a chunk
    /// boundary); a second call is a no-op.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.flush_full_chunks()?;
        self.port.send(&self.buf, true)?;
        self.buf.clear();
        self.finished = true;
        Ok(())
    }

    /// Recover the underlying port (e.g. to close it after [`finish`](Self::finish)).
    pub fn into_port(self) -> P {
        self.port
    }
}

impl<P: MsgPort> ByteSink for ChannelSink<P> {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        self.flush_full_chunks()
    }
}

/// Reassembles a framed stream from a [`MsgPort`] into one contiguous buffer, ready for
/// [`TableReader`](crate::table::TableReader).
pub struct ChannelReceiver<P: MsgPort> {
    port: P,
}

impl<P: MsgPort> ChannelReceiver<P> {
    /// Wrap `port`.
    pub fn new(port: P) -> Self {
        ChannelReceiver { port }
    }

    /// Receive messages until the `last` marker, returning the reassembled byte stream.
    /// Propagates [`WireError::PeerClosed`] if the producer closed before finishing, and
    /// fails with [`WireError::TooLarge`] as soon as the stream exceeds `max` bytes.
    pub fn receive(&mut self, max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let last = self.port.recv(&mut out)?;
            if out.len() > max {
                return Err(WireError::TooLarge);
            }
            if last {
                return Ok(out);
            }
        }
    }

    /// Recover the underlying port.
    pub fn into_port(self) -> P {
        self.port
    }
}

#[cfg(feature = "io")]
pub use self::io_port::{IpcPort, IpcPortRef};
#[cfg(feature = "io")]
pub(crate) use self::io_port::{recv_msg, send_msg};

/// The concrete [`MsgPort`] over a real IPC channel endpoint, behind the `io` feature so
/// the framing above stays dependency-free.
#[cfg(feature = "io")]
mod io_port {
    use super::MsgPort;
    use crate::wire::{Result, WireError};
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use libkern::{KError, RawHandle, Rights};
    use libos::{Carried, Channel, Deadline, Handle, Msg, Only, Ref, WaitSet, block_on};

    /// Transport flag in the IPC header's `flags`: the final message of a stream. The
    /// kernel passes `flags` through untouched.
    const MSG_FLAG_LAST: u16 = 1 << 0;

    /// The endpoint a port sends and receives on.
    enum End {
        /// One the port owns, closed with it.
        Owned(Handle<Channel, Only>),
        /// One its caller owns and keeps open while the port lives.
        Borrowed(u64),
    }

    impl End {
        fn as_ref(&self) -> Ref<'_, Channel, Only> {
            match self {
                End::Owned(h) => h.as_ref(),
                // SAFETY: `IpcPort::new`'s caller keeps the endpoint open while the port lives.
                End::Borrowed(raw) => unsafe { Ref::from_raw(RawHandle(*raw), Rights::empty()) },
            }
        }
    }

    /// A [`MsgPort`] over one IPC channel endpoint, owning a reusable one-page message
    /// buffer. A full ring blocks the send; a closed peer fails it.
    pub struct IpcPort {
        end: End,
        buf: Box<Msg>,
    }

    impl IpcPort {
        /// A port that owns `endpoint` and closes it when dropped.
        pub fn owning(endpoint: Handle<Channel, Only>) -> Self {
            IpcPort { end: End::Owned(endpoint), buf: Box::new(Msg::new()) }
        }

        /// Wrap a channel-endpoint handle the caller keeps owning: the port closes nothing. For
        /// callers not yet on libos; the typed form is [`owning`](Self::owning).
        // TODO(raw-handle-shims): goes when its last caller is on libos.
        pub fn new(endpoint: u64) -> Self {
            IpcPort { end: End::Borrowed(endpoint), buf: Box::new(Msg::new()) }
        }

        /// The endpoint this port sends and receives on.
        pub fn channel(&self) -> Ref<'_, Channel, Only> {
            self.end.as_ref()
        }

        /// The endpoint handle this port sends/receives on, still owned by whoever owned it. For
        /// callers not yet on libos; the typed form is [`channel`](Self::channel).
        // TODO(raw-handle-shims): goes when its last caller is on libos.
        pub fn endpoint(&self) -> u64 {
            self.channel().raw().0
        }

        /// As [`MsgPort::send`], giving up once the ring has stayed full to `deadline`: a message
        /// held that long is dropped, and the send fails [`WireError::SinkFull`].
        pub fn send_by(&mut self, payload: &[u8], last: bool, deadline: Deadline) -> Result<()> {
            self.buf.set(payload).map_err(|_| WireError::SinkFull)?;
            self.buf.set_flags(if last { MSG_FLAG_LAST } else { 0 });
            send_msg(self.channel(), &self.buf, &mut Carried::new(), deadline)
        }

        /// As [`MsgPort::recv`], giving up at `deadline`: `None` if no message came by then.
        pub fn recv_by(&mut self, out: &mut Vec<u8>, deadline: Deadline) -> Result<Option<bool>> {
            // Whatever handles the message brought close as they drop: a frame carries none.
            if recv_msg(self.end.as_ref(), &mut self.buf, deadline)?.is_none() {
                return Ok(None);
            }
            out.extend_from_slice(self.buf.payload());
            Ok(Some(self.buf.flags() & MSG_FLAG_LAST != 0))
        }
    }

    /// How a transport refusal reads on the wire: a closed peer is `PeerClosed`; a full ring or
    /// anything else, `SinkFull`.
    pub(crate) fn map_err(e: libos::Error) -> WireError {
        match e.code() {
            KError::PeerClosed => WireError::PeerClosed,
            _ => WireError::SinkFull,
        }
    }

    /// Send `msg` and `handles` on `ep`, waiting for the peer to take it: for ever on
    /// [`Deadline::NEVER`], else until `deadline`, when it is dropped with its handles and the
    /// send fails `SinkFull`. The handles move once the kernel holds the message, whatever the
    /// wait then says; a refusal before that leaves them in `handles`.
    pub(crate) fn send_msg(
        ep: Ref<'_, Channel, Only>,
        msg: &Msg,
        handles: &mut Carried,
        deadline: Deadline,
    ) -> Result<()> {
        let op =
            if deadline == Deadline::NEVER { ep.send_held(msg, handles) } else { ep.send_by(msg, handles, deadline) };
        block_on(op.map_err(map_err)?).map(|_| ()).map_err(map_err)
    }

    /// Receive into `msg` from `ep`, waiting for one if the ring is empty: `None` if none came by
    /// `deadline`. `PeerClosed` once the peer has gone and nothing is left.
    pub(crate) fn recv_msg(ep: Ref<'_, Channel, Only>, msg: &mut Msg, deadline: Deadline) -> Result<Option<Carried>> {
        loop {
            match ep.recv(msg) {
                Ok(handles) => return Ok(Some(handles)),
                Err(e) if e.code() == KError::WouldBlock => {
                    let mut set = WaitSet::<1>::new();
                    set.add(ep).map_err(|_| WireError::PeerClosed)?;
                    match set.wait(deadline) {
                        Ok(s) if s.is_empty() && deadline != Deadline::NEVER => return Ok(None),
                        Ok(_) => {}
                        Err(_) => return Err(WireError::PeerClosed),
                    }
                }
                Err(e) => return Err(map_err(e)),
            }
        }
    }

    impl MsgPort for IpcPort {
        fn send(&mut self, payload: &[u8], last: bool) -> Result<()> {
            self.send_by(payload, last, Deadline::NEVER)
        }

        fn recv(&mut self, out: &mut Vec<u8>) -> Result<bool> {
            self.recv_by(out, Deadline::NEVER).map(|last| last.unwrap_or(false))
        }
    }

    /// An [`IpcPort`] over an endpoint its owner keeps: the port closes nothing, so a stream lent
    /// for a write stays open after it.
    pub struct IpcPortRef<'a> {
        end: Ref<'a, Channel, Only>,
        buf: Box<Msg>,
    }

    impl<'a> IpcPortRef<'a> {
        /// A port on `end`, borrowed.
        pub fn new(end: impl Into<Ref<'a, Channel, Only>>) -> Self {
            IpcPortRef { end: end.into(), buf: Box::new(Msg::new()) }
        }
    }

    impl MsgPort for IpcPortRef<'_> {
        fn send(&mut self, payload: &[u8], last: bool) -> Result<()> {
            self.buf.set(payload).map_err(|_| WireError::SinkFull)?;
            self.buf.set_flags(if last { MSG_FLAG_LAST } else { 0 });
            send_msg(self.end, &self.buf, &mut Carried::new(), Deadline::NEVER)
        }

        fn recv(&mut self, out: &mut Vec<u8>) -> Result<bool> {
            // Whatever handles the message brought close as they drop: a frame carries none.
            recv_msg(self.end, &mut self.buf, Deadline::NEVER)?;
            out.extend_from_slice(self.buf.payload());
            Ok(self.buf.flags() & MSG_FLAG_LAST != 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Item, TableReader, TableWriter};
    use crate::wire::{Schema, StreamFlags, TypeModifiers, TypeTag, Value, WireError};
    use alloc::string::String;

    /// An in-memory loopback: `send` appends framed messages to a queue that `recv`
    /// drains — enough to exercise the framing without real syscalls.
    #[derive(Default)]
    struct Loopback {
        msgs: Vec<(Vec<u8>, bool)>,
        read: usize,
    }

    impl MsgPort for Loopback {
        fn send(&mut self, payload: &[u8], last: bool) -> Result<()> {
            self.msgs.push((payload.to_vec(), last));
            Ok(())
        }
        fn recv(&mut self, out: &mut Vec<u8>) -> Result<bool> {
            if self.read >= self.msgs.len() {
                return Err(WireError::PeerClosed); // producer never finished
            }
            let (payload, last) = &self.msgs[self.read];
            self.read += 1;
            out.extend_from_slice(payload);
            Ok(*last)
        }
    }

    /// A port whose `send` fails with `PeerClosed` after `ok` successful sends — models
    /// a consumer that closed its read end mid-stream.
    struct CloseAfter {
        ok: usize,
    }
    impl MsgPort for CloseAfter {
        fn send(&mut self, _payload: &[u8], _last: bool) -> Result<()> {
            if self.ok == 0 {
                return Err(WireError::PeerClosed);
            }
            self.ok -= 1;
            Ok(())
        }
        fn recv(&mut self, _out: &mut Vec<u8>) -> Result<bool> {
            unreachable!("send-only test")
        }
    }

    #[test]
    fn single_short_stream_is_one_last_message() {
        let mut sink = ChannelSink::new(Loopback::default(), 8);
        sink.put(b"hello").unwrap(); // < max_chunk: nothing sent yet
        assert!(sink.port.msgs.is_empty());
        sink.finish().unwrap();
        let port = sink.into_port();
        assert_eq!(port.msgs.len(), 1);
        assert_eq!(port.msgs[0], (b"hello".to_vec(), true));
    }

    #[test]
    fn long_stream_chunks_eagerly_then_final_last() {
        let mut sink = ChannelSink::new(Loopback::default(), 8);
        sink.put(&[0u8; 20]).unwrap();
        assert_eq!(sink.port.msgs.len(), 2);
        assert!(sink.port.msgs.iter().all(|(p, last)| p.len() == 8 && !*last));
        sink.finish().unwrap();
        let port = sink.into_port();
        assert_eq!(port.msgs.len(), 3);
        assert_eq!(port.msgs[2].0.len(), 4);
        assert!(port.msgs[2].1); // last
    }

    #[test]
    fn boundary_aligned_stream_ends_with_empty_last() {
        let mut sink = ChannelSink::new(Loopback::default(), 8);
        sink.put(&[7u8; 16]).unwrap();
        sink.finish().unwrap();
        let port = sink.into_port();
        assert_eq!(port.msgs.len(), 3);
        assert_eq!(port.msgs[2], (Vec::new(), true));
    }

    #[test]
    fn receiver_reassembles_across_chunks() {
        let mut sink = ChannelSink::new(Loopback::default(), 4);
        let payload: Vec<u8> = (0..=250u8).collect();
        sink.put(&payload).unwrap();
        sink.finish().unwrap();
        let mut rx = ChannelReceiver::new(sink.into_port());
        assert_eq!(rx.receive(usize::MAX).unwrap(), payload);
    }

    #[test]
    fn full_tsm1_stream_round_trips_over_the_transport() {
        let schema = Schema::new().field("pid", TypeTag::Int, TypeModifiers::NONE).field(
            "name",
            TypeTag::String,
            TypeModifiers::NONE,
        );
        let mut tw = TableWriter::new(ChannelSink::new(Loopback::default(), 7));
        tw.write_schema(StreamFlags::NONE, &schema).unwrap();
        tw.write_row(&[Value::Int(1), Value::Str(String::from("init"))]).unwrap();
        tw.write_row(&[Value::Int(2), Value::Str(String::from("fs-server"))]).unwrap();
        tw.finish_with_status(0).unwrap();
        let mut sink = tw.into_sink();
        sink.finish().unwrap();

        let bytes = ChannelReceiver::new(sink.into_port()).receive(usize::MAX).unwrap();
        let mut tr = TableReader::new(&bytes).unwrap();
        assert_eq!(tr.schema(), &schema);
        assert!(matches!(tr.next(), Some(Ok(Item::Row(r))) if r[0] == Value::Int(1)));
        assert!(matches!(tr.next(), Some(Ok(Item::Row(r))) if r[0] == Value::Int(2)));
        assert!(matches!(tr.next(), Some(Ok(Item::End(0)))));
    }

    #[test]
    fn peer_closed_surfaces_to_the_producer() {
        let mut sink = ChannelSink::new(CloseAfter { ok: 1 }, 4);
        assert_eq!(sink.put(&[0u8; 4]).unwrap(), ()); // first chunk sent OK
        assert_eq!(sink.put(&[0u8; 4]), Err(WireError::PeerClosed));
    }

    #[test]
    fn receiver_reports_peer_closed_if_producer_never_finishes() {
        let mut port = Loopback::default();
        port.send(b"partial", false).unwrap();
        assert_eq!(ChannelReceiver::new(port).receive(usize::MAX), Err(WireError::PeerClosed));
    }

    struct Endless {
        recvs: usize,
        total: usize,
    }

    impl MsgPort for Endless {
        fn send(&mut self, _payload: &[u8], _last: bool) -> Result<()> {
            Ok(())
        }
        fn recv(&mut self, out: &mut Vec<u8>) -> Result<bool> {
            self.recvs += 1;
            out.extend_from_slice(&[0u8; 1024]);
            Ok(self.recvs == self.total)
        }
    }

    #[test]
    fn receive_stops_once_the_stream_passes_the_cap() {
        let mut rx = ChannelReceiver::new(Endless { recvs: 0, total: 1000 });
        assert_eq!(rx.receive(64 * 1024), Err(WireError::TooLarge));
        assert!(rx.into_port().recvs <= 65);
    }

    /// The cap holds on the last message too: a stream ending one byte past it is refused, and
    /// one ending on it is not.
    #[test]
    fn receive_holds_the_cap_on_the_last_message() {
        let mut rx = ChannelReceiver::new(Endless { recvs: 0, total: 10 });
        assert_eq!(rx.receive(10 * 1024 - 1), Err(WireError::TooLarge));
        let mut rx = ChannelReceiver::new(Endless { recvs: 0, total: 10 });
        assert_eq!(rx.receive(10 * 1024).map(|b| b.len()), Ok(10 * 1024));
    }

    #[test]
    fn receive_returns_everything_under_a_cap_above_the_total() {
        let mut rx = ChannelReceiver::new(Endless { recvs: 0, total: 1000 });
        assert_eq!(rx.receive(1000 * 1024).unwrap().len(), 1000 * 1024);
    }
}
