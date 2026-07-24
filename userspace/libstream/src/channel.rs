//! Channel transport — carry a TSM1 byte stream across an IPC channel.
//!
//! The [`wire`](crate::wire) codec is transport-agnostic (it writes a [`ByteSink`] and
//! reads a [`ByteSource`]). This module is the adapter that moves those bytes over a
//! **message-oriented** IPC channel: a producer's [`ChannelSink`] frames the stream into
//! fixed-size messages (so an unbounded producer streams rather than buffering the whole
//! thing), and a consumer's [`ChannelReceiver`] reassembles them back into one buffer for
//! [`TableReader`](crate::table::TableReader).
//!
//! ## The [`MsgPort`] seam
//!
//! The framing logic here is **pure** — it depends only on `core + alloc` and a
//! [`MsgPort`] (send/recv one framed message). The concrete port over
//! `sys_channel_send`/`sys_channel_recv` (the only syscall-touching part) is a thin
//! `libkern` impl that lands with the pipe-wiring helpers (its first consumer); the
//! framing here host-tests against a mock port. This keeps `libstream`'s wire core
//! dependency-free (see the crate `Cargo.toml`).
//!
//! ## Framing
//!
//! The stream is split into chunks of at most `max_chunk` bytes, each sent as one
//! message; the **final** message carries a `last` marker (in the real port, an
//! `IpcMsgHeader.flags` bit). A stream always ends with exactly one `last` message —
//! even an empty tail — so the receiver has an unambiguous end-of-stream. Chunk
//! boundaries are byte-arbitrary: a record may straddle two messages; the receiver
//! rejoins the bytes before any parsing (v1 reassembles the whole stream first —
//! incremental parsing is a later refinement, as with [`TableReader`]).
//!
//! ## Backpressure & early close
//!
//! A [`MsgPort::send`] blocks while the channel's bounded queue is full (the kernel's
//! backpressure), so a fast producer paces itself to a slow consumer with no
//! shell-level flow control. If the consumer closes its read end, the next `send`
//! returns [`WireError::PeerClosed`]; a producer treats that as "stop producing, exit
//! cleanly" (design §1 — the `yes | head -1` case). No signals.

use alloc::vec::Vec;

use crate::wire::{ByteSink, Result};

/// A message-oriented transport for one TSM1 stream: send/receive one framed message
/// at a time. The framing ([`ChannelSink`]/[`ChannelReceiver`]) is generic over this so
/// the byte-level logic is testable against a mock, independent of the real IPC syscalls.
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
/// eager-flushing full chunks as they accumulate (so an unbounded stream never buffers
/// more than one chunk). Call [`finish`](Self::finish) once the stream is complete to
/// emit the terminating `last` message.
///
/// Typical use wraps it in a [`TableWriter`](crate::table::TableWriter):
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
        ChannelSink {
            port,
            buf: Vec::new(),
            max_chunk,
            finished: false,
        }
    }

    /// Emit every buffered full chunk as a (non-`last`) message, leaving `< max_chunk`
    /// bytes buffered for the next write or [`finish`](Self::finish).
    fn flush_full_chunks(&mut self) -> Result<()> {
        while self.buf.len() >= self.max_chunk {
            self.port.send(&self.buf[..self.max_chunk], false)?;
            self.buf.drain(..self.max_chunk);
        }
        Ok(())
    }

    /// Send the buffered tail as the terminating `last` message, closing the stream.
    /// Emits exactly one `last` message (an empty one if the stream ended on a chunk
    /// boundary), so the receiver always sees an unambiguous end. Idempotent-safe: a
    /// second call is a no-op.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        // Drain any remaining full chunks first (in case bytes were buffered without a
        // put crossing the threshold), then the final short tail as the `last` message.
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

/// Reassembles a framed stream from a [`MsgPort`] back into one contiguous buffer, ready
/// for [`TableReader`](crate::table::TableReader). v1 collects the whole stream before
/// parsing (incremental parsing is a later refinement).
pub struct ChannelReceiver<P: MsgPort> {
    port: P,
}

impl<P: MsgPort> ChannelReceiver<P> {
    /// Wrap `port`.
    pub fn new(port: P) -> Self {
        ChannelReceiver { port }
    }

    /// Receive messages until the `last` marker, returning the reassembled byte stream.
    /// Propagates [`WireError::PeerClosed`] if the producer closed before finishing.
    pub fn receive(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            if self.port.recv(&mut out)? {
                return Ok(out);
            }
        }
    }

    /// Recover the underlying port.
    pub fn into_port(self) -> P {
        self.port
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
        // 20 bytes, max_chunk 8 → chunks of 8, 8, then a 4-byte `last`.
        let mut sink = ChannelSink::new(Loopback::default(), 8);
        sink.put(&[0u8; 20]).unwrap();
        // Two full chunks flushed eagerly; none is `last` yet.
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
        // Exactly 2 chunks of data → the terminator is an empty `last` message, so the
        // receiver still gets an unambiguous end.
        let mut sink = ChannelSink::new(Loopback::default(), 8);
        sink.put(&[7u8; 16]).unwrap();
        sink.finish().unwrap();
        let port = sink.into_port();
        assert_eq!(port.msgs.len(), 3);
        assert_eq!(port.msgs[2], (Vec::new(), true));
    }

    #[test]
    fn receiver_reassembles_across_chunks() {
        // Producer frames a byte pattern across several small chunks; receiver rejoins.
        let mut sink = ChannelSink::new(Loopback::default(), 4);
        let payload: Vec<u8> = (0..=250u8).collect();
        sink.put(&payload).unwrap();
        sink.finish().unwrap();
        let mut rx = ChannelReceiver::new(sink.into_port());
        assert_eq!(rx.receive().unwrap(), payload);
    }

    #[test]
    fn full_tsm1_stream_round_trips_over_the_transport() {
        let schema = Schema::new()
            .field("pid", TypeTag::Int, TypeModifiers::NONE)
            .field("name", TypeTag::String, TypeModifiers::NONE);
        // Small max_chunk forces multi-message framing of a real TSM1 stream.
        let mut tw = TableWriter::new(ChannelSink::new(Loopback::default(), 7));
        tw.write_schema(StreamFlags::NONE, &schema).unwrap();
        tw.write_row(&[Value::Int(1), Value::Str(String::from("init"))]).unwrap();
        tw.write_row(&[Value::Int(2), Value::Str(String::from("fs-server"))]).unwrap();
        tw.finish_with_status(0).unwrap();
        let mut sink = tw.into_sink();
        sink.finish().unwrap();

        let bytes = ChannelReceiver::new(sink.into_port()).receive().unwrap();
        let mut tr = TableReader::new(&bytes).unwrap();
        assert_eq!(tr.schema(), &schema);
        assert!(matches!(tr.next(), Some(Ok(Item::Row(r))) if r[0] == Value::Int(1)));
        assert!(matches!(tr.next(), Some(Ok(Item::Row(r))) if r[0] == Value::Int(2)));
        assert!(matches!(tr.next(), Some(Ok(Item::End(0)))));
    }

    #[test]
    fn peer_closed_surfaces_to_the_producer() {
        // The consumer closes after accepting one message; the next flush fails loud so
        // the producer can stop cleanly.
        let mut sink = ChannelSink::new(CloseAfter { ok: 1 }, 4);
        assert_eq!(sink.put(&[0u8; 4]).unwrap(), ()); // first chunk sent OK
        // Second full chunk → peer already closed → PeerClosed.
        assert_eq!(sink.put(&[0u8; 4]), Err(WireError::PeerClosed));
    }

    #[test]
    fn receiver_reports_peer_closed_if_producer_never_finishes() {
        // A Loopback with a non-`last` message and nothing more models a producer that
        // died mid-stream; the receiver surfaces PeerClosed rather than hanging.
        let mut port = Loopback::default();
        port.send(b"partial", false).unwrap();
        assert_eq!(ChannelReceiver::new(port).receive(), Err(WireError::PeerClosed));
    }
}
