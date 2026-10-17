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

use crate::wire::{ByteSink, Result};

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

#[cfg(feature = "io")]
pub use self::io_port::IpcPort;

/// The concrete [`MsgPort`] over a real IPC channel endpoint, behind the `io` feature so
/// the framing above stays dependency-free.
#[cfg(feature = "io")]
mod io_port {
    use super::MsgPort;
    use crate::wire::{Result, WireError};
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use libkern::abi::{IPC_MSG_SIZE, IPC_PAYLOAD_SIZE, SENDMODE_BLOCK};
    use libkern::syscall::{
        SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_WAIT, syscall1, syscall4, syscall6,
    };

    const E_WOULDBLOCK: i64 = -11;
    const E_PEERCLOSED: i64 = -13;

    // `IpcMsgHeader` field byte offsets (see `libkern::abi::IpcMsgHeader`), so the buffer
    // can be a plain byte array.
    const OFF_PAYLOAD_LEN: usize = 4;
    const OFF_HANDLE_COUNT: usize = 8;
    const OFF_FLAGS: usize = 10;
    const OFF_PAYLOAD: usize = 24;

    /// Transport flag in the IPC header's `flags`: the final message of a stream. The
    /// kernel passes `flags` through untouched.
    const MSG_FLAG_LAST: u16 = 1 << 0;

    /// A [`MsgPort`] over one IPC channel endpoint, owning a reusable one-page message
    /// buffer. A full ring blocks the send; a closed peer fails it.
    pub struct IpcPort {
        endpoint: u64,
        buf: Box<[u8; IPC_MSG_SIZE]>,
    }

    impl IpcPort {
        /// Wrap a channel-endpoint handle.
        pub fn new(endpoint: u64) -> Self {
            IpcPort { endpoint, buf: Box::new([0u8; IPC_MSG_SIZE]) }
        }
        /// The endpoint handle this port sends/receives on.
        pub fn endpoint(&self) -> u64 {
            self.endpoint
        }
    }

    fn map_err(r: i64) -> WireError {
        match r {
            E_PEERCLOSED => WireError::PeerClosed,
            _ => WireError::SinkFull, // WouldBlock / pending-queue-full / other
        }
    }

    impl MsgPort for IpcPort {
        fn send(&mut self, payload: &[u8], last: bool) -> Result<()> {
            if payload.len() > IPC_PAYLOAD_SIZE {
                return Err(WireError::SinkFull);
            }
            let flags: u16 = if last { MSG_FLAG_LAST } else { 0 };
            self.buf[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            self.buf[OFF_HANDLE_COUNT] = 0;
            self.buf[OFF_FLAGS..OFF_FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
            self.buf[OFF_PAYLOAD..OFF_PAYLOAD + payload.len()].copy_from_slice(payload);

            let no_handles = [0u64; 1];
            // Block-mode send returns a PendingOperation handle (delivery; a full ring
            // blocks here) or a negative `KError`; a handle means the message is committed.
            // SAFETY: `buf` is a valid IPC_MSG_SIZE buffer;
            // `no_handles` is a valid (empty) handle array with count 0.
            let r = unsafe {
                syscall6(
                    SYS_CHANNEL_SEND,
                    self.endpoint,
                    self.buf.as_ptr() as u64,
                    no_handles.as_ptr() as u64,
                    0,
                    SENDMODE_BLOCK,
                    u64::MAX,
                )
            };
            if r < 0 {
                return Err(map_err(r));
            }
            // Wait for the delivery PO, then release it.
            let po = r as u64;
            let wait_handles = [po];
            let mut result = [0u8; 24]; // one IoResult
            // SAFETY: valid handle + result out-buffers.
            let waited =
                unsafe { syscall4(SYS_WAIT, wait_handles.as_ptr() as u64, 1, result.as_mut_ptr() as u64, u64::MAX) };
            // SAFETY: closing the PO handle we own.
            unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
            if waited < 1 {
                return Err(WireError::PeerClosed);
            }
            // IoResult.status is an i32 at offset 8; negative ⇒ the peer died while queued.
            let status = i32::from_le_bytes([result[8], result[9], result[10], result[11]]);
            if status < 0 {
                return Err(map_err(status as i64));
            }
            Ok(())
        }

        fn recv(&mut self, out: &mut Vec<u8>) -> Result<bool> {
            let mut recv_handles = [0u64; 8];
            let mut count: usize = 0;
            loop {
                // SAFETY: valid msg/handles/count out-params.
                let r = unsafe {
                    syscall4(
                        SYS_CHANNEL_RECV,
                        self.endpoint,
                        self.buf.as_mut_ptr() as u64,
                        recv_handles.as_mut_ptr() as u64,
                        (&raw mut count) as u64,
                    )
                };
                if r == 0 {
                    let n = u32::from_le_bytes([
                        self.buf[OFF_PAYLOAD_LEN],
                        self.buf[OFF_PAYLOAD_LEN + 1],
                        self.buf[OFF_PAYLOAD_LEN + 2],
                        self.buf[OFF_PAYLOAD_LEN + 3],
                    ]) as usize;
                    if n > IPC_PAYLOAD_SIZE {
                        return Err(WireError::UnexpectedEof);
                    }
                    out.extend_from_slice(&self.buf[OFF_PAYLOAD..OFF_PAYLOAD + n]);
                    let flags = u16::from_le_bytes([self.buf[OFF_FLAGS], self.buf[OFF_FLAGS + 1]]);
                    return Ok(flags & MSG_FLAG_LAST != 0);
                }
                if r == E_WOULDBLOCK {
                    // Empty ring: block until a message (or the peer's close) signals it.
                    let wait_handles = [self.endpoint];
                    let mut result = [0u8; 24];
                    // SAFETY: valid handle + result out-buffers.
                    let waited = unsafe {
                        syscall4(SYS_WAIT, wait_handles.as_ptr() as u64, 1, result.as_mut_ptr() as u64, u64::MAX)
                    };
                    if waited < 1 {
                        return Err(WireError::PeerClosed);
                    }
                    continue;
                }
                return Err(map_err(r));
            }
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
        assert_eq!(rx.receive().unwrap(), payload);
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

        let bytes = ChannelReceiver::new(sink.into_port()).receive().unwrap();
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
        assert_eq!(ChannelReceiver::new(port).receive(), Err(WireError::PeerClosed));
    }
}
