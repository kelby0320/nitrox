//! The process **setup message** — how a shell hands a pipeline stage its standard
//! streams and `argv`. See `docs/spec/pipeline-stdio.md`.
//!
//! This is the pure, transport-agnostic half of the C3 stdio convention: the `arg0`
//! bootstrap descriptor, the TSM1 payload codec, and the mapping between a stream
//! **presence bitmap** and the ordered transferred handles. Both the sender (the shell)
//! and the receiver (`libos::bootstrap().setup()`, the stage) share these; the actual
//! IPC send/recv is thin syscall glue layered on top.
//!
//! ## Tiers
//!
//! A child is spawned with `arg0` set by [`bootstrap_arg0`]. `arg0 == 0` — the value
//! every non-stage spawner passes — means **Tier 0**: register-only, no setup message
//! ([`setup_is_pending`] is `false`). A Tier-1 stage gets a versioned descriptor with
//! [`SETUP_PENDING`](self) set, and its runtime receives one setup message.
//!
//! ## Message
//!
//! The stream endpoints ride as transferred handles in the IPC message, packed in the
//! canonical order **stdin, stdout, stderr** (only those present — see [`Streams`]). The
//! payload is a TSM1 `Record { streams: Int, argv: List<String> }` ([`SetupPayload`]);
//! `env` may be appended as a later field without a version bump (readers ignore fields
//! they don't know).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::wire::{
    ByteSource, Record, Result, Schema, TypeModifiers, TypeTag, Value, WireError, read_value,
    write_value,
};

// --- The `arg0` bootstrap descriptor ---------------------------------------

/// The bootstrap-descriptor version this build speaks (the low byte of a Tier-1 `arg0`).
pub const BOOTSTRAP_VERSION: u8 = 1;

/// `arg0` bit 8 — a setup message is queued on the child's bootstrap endpoint.
const SETUP_PENDING: u64 = 1 << 8;

/// The `arg0` a parent passes to `sys_process_spawn` to launch a Tier-1 stage: the
/// descriptor version, plus [`SETUP_PENDING`](self) when a setup message will follow on
/// the bootstrap endpoint. (A Tier-0 child takes `arg0 == 0`.)
pub fn bootstrap_arg0(setup_pending: bool) -> u64 {
    let mut a = BOOTSTRAP_VERSION as u64;
    if setup_pending {
        a |= SETUP_PENDING;
    }
    a
}

/// Whether `arg0` is a recognised bootstrap descriptor asking the runtime to receive a
/// setup message. `arg0 == 0` (Tier 0) and any descriptor with a different version are
/// `false` — a stage that isn't expecting a setup message never blocks on one.
pub fn setup_is_pending(arg0: u64) -> bool {
    (arg0 & 0xFF) as u8 == BOOTSTRAP_VERSION && arg0 & SETUP_PENDING != 0
}

// --- Stream presence + handle mapping --------------------------------------

/// Presence bit for `stdin` in a [`Streams`] bitmap.
pub const STREAM_STDIN: u32 = 1 << 0;
/// Presence bit for `stdout`.
pub const STREAM_STDOUT: u32 = 1 << 1;
/// Presence bit for `stderr`.
pub const STREAM_STDERR: u32 = 1 << 2;

/// The three defined stream bits.
const STREAM_MASK: u32 = STREAM_STDIN | STREAM_STDOUT | STREAM_STDERR;

/// A stage's standard-stream handles. Each is optional — a *source* stage has no
/// `stdin`, a *sink* no `stdout`, and `stderr` is a shared diagnostic sink. The handles
/// travel as transferred IPC handles; this type pairs them with the presence
/// [`bitmap`](Self::bitmap) so each ends up in the right slot.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Streams {
    /// The read end of the upstream pipe, if any.
    pub stdin: Option<u64>,
    /// The write end of the downstream pipe, if any.
    pub stdout: Option<u64>,
    /// The shared diagnostic sink, if any.
    pub stderr: Option<u64>,
}

impl Streams {
    /// The presence bitmap: which of `stdin`/`stdout`/`stderr` are set.
    pub fn bitmap(&self) -> u32 {
        let mut b = 0;
        if self.stdin.is_some() {
            b |= STREAM_STDIN;
        }
        if self.stdout.is_some() {
            b |= STREAM_STDOUT;
        }
        if self.stderr.is_some() {
            b |= STREAM_STDERR;
        }
        b
    }

    /// The present handles in canonical order (`stdin`, `stdout`, `stderr`) — the order
    /// the sender packs them into the message and [`from_bitmap`](Self::from_bitmap)
    /// reads them back.
    pub fn ordered(&self) -> Vec<u64> {
        [self.stdin, self.stdout, self.stderr]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Pair a presence `bitmap` with the ordered transferred `handles`, producing the
    /// named streams. Errors ([`WireError::SchemaMismatch`]) if `bitmap` sets an unknown
    /// bit or `handles.len()` disagrees with the number of present streams.
    pub fn from_bitmap(bitmap: u32, handles: &[u64]) -> Result<Streams> {
        if bitmap & !STREAM_MASK != 0 {
            return Err(WireError::SchemaMismatch);
        }
        if handles.len() != (bitmap & STREAM_MASK).count_ones() as usize {
            return Err(WireError::SchemaMismatch);
        }
        let mut it = handles.iter().copied();
        let stdin = if bitmap & STREAM_STDIN != 0 { it.next() } else { None };
        let stdout = if bitmap & STREAM_STDOUT != 0 { it.next() } else { None };
        let stderr = if bitmap & STREAM_STDERR != 0 { it.next() } else { None };
        Ok(Streams { stdin, stdout, stderr })
    }
}

// --- The setup-message payload ---------------------------------------------

/// The decoded setup-message payload: the stream presence [`bitmap`](Streams::bitmap)
/// and `argv`. The stream *handles* are carried out-of-band as transferred IPC handles
/// and reunited with the bitmap via [`Streams::from_bitmap`].
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SetupPayload {
    /// The stream presence bitmap (matches the packed transferred handles).
    pub streams: u32,
    /// Command-line arguments; `argv[0]` is the program name by convention.
    pub argv: Vec<String>,
}

impl SetupPayload {
    /// Encode as a TSM1 `Record { streams: Int, argv: List<String> }`.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let schema = Schema::new()
            .field("streams", TypeTag::Int, TypeModifiers::NONE)
            .field("argv", TypeTag::List, TypeModifiers::NONE);
        let argv: Vec<Value> = self.argv.iter().map(|s| Value::Str(s.clone())).collect();
        let record = Record {
            schema,
            values: vec![Value::Int(self.streams as i64), Value::List(Arc::from(argv))],
        };
        let mut buf = Vec::new();
        write_value(&mut buf, &Value::Record(Arc::new(record)))?;
        Ok(buf)
    }

    /// Decode a payload written by [`encode`](Self::encode). Reads the first two fields
    /// by position (`streams`, `argv`) and ignores any later ones, so a newer sender can
    /// append `env` without breaking an older reader.
    pub fn decode(bytes: &[u8]) -> Result<SetupPayload> {
        let mut src = ByteSource::new(bytes);
        let value = read_value(&mut src, TypeTag::Record)?;
        let record = value.as_record().ok_or(WireError::SchemaMismatch)?;
        if record.values.len() < 2 {
            return Err(WireError::SchemaMismatch);
        }
        let streams = record.values[0].as_int().ok_or(WireError::SchemaMismatch)? as u32;
        let list = record.values[1].as_list().ok_or(WireError::SchemaMismatch)?;
        let mut argv = Vec::with_capacity(list.len());
        for item in list {
            argv.push(String::from(item.as_str().ok_or(WireError::SchemaMismatch)?));
        }
        Ok(SetupPayload { streams, argv })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg0_descriptor_round_trips() {
        // Tier 0: the value every current spawner passes.
        assert!(!setup_is_pending(0));
        // Tier 1 with / without a pending setup message.
        assert!(setup_is_pending(bootstrap_arg0(true)));
        assert!(!setup_is_pending(bootstrap_arg0(false)));
        // A descriptor with a different version is not honoured (forward-safety).
        let other_version = (bootstrap_arg0(true) & !0xFF) | 0x02;
        assert!(!setup_is_pending(other_version));
    }

    #[test]
    fn streams_bitmap_and_mapping_round_trip() {
        for s in [
            Streams { stdin: Some(0x10), stdout: Some(0x11), stderr: Some(0x12) },
            Streams { stdin: None, stdout: Some(0x21), stderr: Some(0x22) }, // source stage
            Streams { stdin: Some(0x30), stdout: None, stderr: Some(0x31) }, // sink stage
            Streams { stdin: Some(0x40), stdout: Some(0x41), stderr: None },
            Streams::default(),
        ] {
            let handles = s.ordered();
            assert_eq!(handles.len(), s.bitmap().count_ones() as usize);
            assert_eq!(Streams::from_bitmap(s.bitmap(), &handles).unwrap(), s);
        }
    }

    #[test]
    fn from_bitmap_rejects_bad_input() {
        // Handle count disagrees with the bitmap.
        assert_eq!(
            Streams::from_bitmap(STREAM_STDIN | STREAM_STDOUT, &[0x1]),
            Err(WireError::SchemaMismatch)
        );
        // An unknown presence bit.
        assert_eq!(
            Streams::from_bitmap(1 << 5, &[0x1]),
            Err(WireError::SchemaMismatch)
        );
    }

    #[test]
    fn setup_payload_round_trips() {
        for argv in [
            vec![],
            vec![String::from("list")],
            vec![String::from("copy"), String::from("/a"), String::from("/b")],
            vec![String::from("echo"), String::from(""), String::from("héllo")],
        ] {
            let p = SetupPayload { streams: STREAM_STDOUT | STREAM_STDERR, argv };
            let bytes = p.encode().unwrap();
            assert_eq!(SetupPayload::decode(&bytes).unwrap(), p);
        }
    }

    #[test]
    fn decode_tolerates_a_trailing_field() {
        // A future sender appends `env` as a third field; an older decoder still reads
        // `streams`/`argv` and ignores it.
        let schema = Schema::new()
            .field("streams", TypeTag::Int, TypeModifiers::NONE)
            .field("argv", TypeTag::List, TypeModifiers::NONE)
            .field("env", TypeTag::String, TypeModifiers::NONE);
        let record = Record {
            schema,
            values: vec![
                Value::Int(STREAM_STDIN as i64),
                Value::List(Arc::from(vec![Value::Str(String::from("sort"))])),
                Value::Str(String::from("PATH=/bin")),
            ],
        };
        let mut bytes = Vec::new();
        write_value(&mut bytes, &Value::Record(Arc::new(record))).unwrap();
        let p = SetupPayload::decode(&bytes).unwrap();
        assert_eq!(p.streams, STREAM_STDIN);
        assert_eq!(p.argv, vec![String::from("sort")]);
    }
}
