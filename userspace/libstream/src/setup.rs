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

/// The largest setup payload that fits one IPC message: `IPC_MSG_SIZE` (4096) less the
/// header (24) and the handle slots (8 × 8).
///
/// Defined here rather than imported because the wire core deliberately has **no
/// dependencies** — that is what lets it host-test cleanly, and reaching for `libkern` to
/// get one number would spend that property. The `const` assertion below keeps the two
/// from drifting: under the `io` feature, where a real message is actually sent, a
/// mismatch is a compile error.
pub const SETUP_PAYLOAD_MAX: usize = 4096 - 24 - 8 * 8;

#[cfg(feature = "io")]
const _: () = assert!(SETUP_PAYLOAD_MAX == libkern::abi::IPC_PAYLOAD_SIZE);

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

/// The decoded setup-message payload: the stream presence [`bitmap`](Streams::bitmap),
/// `argv`, and the environment. The stream *handles* are carried out-of-band as
/// transferred IPC handles and reunited with the bitmap via [`Streams::from_bitmap`].
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SetupPayload {
    /// The stream presence bitmap (matches the packed transferred handles).
    pub streams: u32,
    /// Command-line arguments; `argv[0]` is the program name by convention.
    pub argv: Vec<String>,
    /// The environment, as a TSM1 `Record` — **not** `key=value` strings.
    ///
    /// Typed, which removes a bug class rather than merely being tidier: `PATH` is a
    /// `List<String>` instead of a colon-joined string, so a path *containing* a colon is
    /// representable and no two readers can disagree about how to split it. A reader that
    /// expects the wrong type gets a schema mismatch rather than a silent misparse.
    ///
    /// `cwd` is a conventional entry, as `PWD` is in Unix — and safer here, because there
    /// is no second source of truth (no `getcwd`) for it to disagree with.
    ///
    /// Empty for a sender that has none, so "no environment" is not a special case.
    /// See `docs/planning/shell-coreutils-plan.md` § Milestone 3.5 and the decision log,
    /// 2026-07-30.
    pub env: Record,
    /// A **terminal** handle follows the stream handles.
    ///
    /// Not one of [`Streams`], deliberately: a terminal is not a stream. It answers
    /// `SetMode` and delivers `Interrupt`, and a program holds it *as well as* its
    /// stdin/stdout rather than instead of them — `ls | less` inside a terminal has both.
    ///
    /// It exists because `/dev/tty` is a **namespace binding**, one per session, so a
    /// program cannot resolve its way to a *particular* terminal. A terminal emulator hands
    /// the shell it hosts the terminal it attached its backend to, the way Unix inherits
    /// fd 0/1/2 rather than looking them up. See `docs/design/graphical-session.md` §6.1
    /// for what closes this properly.
    pub terminal: bool,
}

/// Split a setup message's transferred handles into its streams and its terminal.
///
/// **Pure, and here rather than in the `io` module, so it can be tested.** It was inline in the
/// receive path, which needs a kernel — and the bound it gets wrong is exactly the sort a host
/// test catches for free: `handles` arrives as a fixed zero-initialised array, so an index
/// inside *it* says nothing about whether a handle was actually transferred. A payload claiming
/// a terminal with none attached read back as `Some(0)`, and a shell would then run its whole
/// REPL against handle 0 (PR #194 review, finding 6).
pub fn split_handles(
    payload: &SetupPayload,
    handles: &[u64],
) -> Result<(Streams, Option<u64>)> {
    let n = (payload.streams & STREAM_MASK).count_ones() as usize;
    if n + usize::from(payload.terminal) > handles.len() {
        return Err(WireError::SchemaMismatch);
    }
    let streams = Streams::from_bitmap(payload.streams, &handles[..n])?;
    let terminal = if payload.terminal { Some(handles[n]) } else { None };
    Ok((streams, terminal))
}

impl SetupPayload {
    /// Encode as a TSM1 `Record { streams: Int, argv: List<String>, env: Record }`.
    ///
    /// `env` is **appended**, never inserted: [`decode`](Self::decode) reads by position,
    /// so an existing stage keeps reading fields 0 and 1 and ignores the rest. Reordering
    /// these fields would break every stage in the image at once.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let schema = Schema::new()
            .field("streams", TypeTag::Int, TypeModifiers::NONE)
            .field("argv", TypeTag::List, TypeModifiers::NONE)
            .field("env", TypeTag::Record, TypeModifiers::NONE)
            .field("terminal", TypeTag::Int, TypeModifiers::NONE);
        let argv: Vec<Value> = self.argv.iter().map(|s| Value::Str(s.clone())).collect();
        let record = Record {
            schema,
            values: vec![
                Value::Int(self.streams as i64),
                Value::List(Arc::from(argv)),
                Value::Record(Arc::new(self.env.clone())),
                Value::Int(self.terminal as i64),
            ],
        };
        let mut buf = Vec::new();
        write_value(&mut buf, &Value::Record(Arc::new(record)))?;
        // The size limit is a property of the *message*, so it is checked here rather
        // than at the send site: that keeps it testable without a kernel, and means every
        // sender gets the same refusal.
        if buf.len() > SETUP_PAYLOAD_MAX {
            return Err(WireError::SetupTooLarge);
        }
        Ok(buf)
    }

    /// Decode a payload written by [`encode`](Self::encode).
    ///
    /// Reads by **position**, and treats every field past `argv` as optional. That is what
    /// makes the format extensible in one direction: a newer sender may append fields an
    /// older reader ignores, and an older sender's message still decodes here — `env`
    /// simply comes back empty. Both directions are tested.
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
        // Absent `env` ⇒ empty, so an older sender is not an error case.
        let env = record
            .values
            .get(2)
            .and_then(|v| v.as_record())
            .cloned()
            .unwrap_or_default();
        // Absent `terminal` ⇒ none, so a sender that predates it is not an error case —
        // the same rule as `env`, one field along.
        let terminal = record.values.get(3).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        Ok(SetupPayload { streams, argv, env, terminal })
    }
}

// --- Syscall-backed stage runtime (Tier-1 spawn + bootstrap) ----------------

#[cfg(feature = "io")]
pub use self::io_stage::{
    Bootstrap, Setup, bootstrap, pipe, send_setup, send_setup_env, send_setup_full,
};

/// The sender (a shell spawning a stage) and receiver (`bootstrap().setup()`) sides of
/// the setup message, over real IPC. Gated behind `io` so the pure protocol above stays
/// host-testable with no dependency. Mirrors the send/recv shape of
/// [`channel::IpcPort`](crate::channel) but carries transferred handles.
#[cfg(feature = "io")]
mod io_stage {
    use super::{SetupPayload, Streams, setup_is_pending};
    use crate::wire::{Record, Result, WireError};
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec::Vec;
    use libkern::abi::{IPC_HANDLE_MAX, IPC_MSG_SIZE, IPC_PAYLOAD_SIZE, SENDMODE_BLOCK};
    use libkern::syscall::{
        SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_WAIT,
        syscall1, syscall4, syscall6,
    };

    const E_WOULDBLOCK: i64 = -11; // KError::WouldBlock
    const E_PEERCLOSED: i64 = -13; // KError::PeerClosed
    const OFF_PAYLOAD_LEN: usize = 4;
    const OFF_HANDLE_COUNT: usize = 8;
    const OFF_PAYLOAD: usize = 24;

    fn map_err(r: i64) -> WireError {
        if r == E_PEERCLOSED {
            WireError::PeerClosed
        } else {
            WireError::SinkFull
        }
    }

    /// Create a pipe (an IPC channel pair) of queue depth `depth`, returning both
    /// endpoint handles `(a, b)`.
    pub fn pipe(depth: u32) -> Result<(u64, u64)> {
        let mut a = 0u64;
        let mut b = 0u64;
        // SAFETY: `a`/`b` are valid writable out-params.
        let r = unsafe {
            syscall4(
                SYS_CHANNEL_CREATE,
                (&raw mut a) as u64,
                (&raw mut b) as u64,
                depth as u64,
                0,
            )
        };
        if r != 0 {
            return Err(WireError::SinkFull);
        }
        Ok((a, b))
    }

    /// Send the **setup message** on `channel` (the shell's end of the stage's bootstrap
    /// endpoint): transfer the present stream handles (canonical order) and carry `argv`.
    /// The stream handles are **moved** to the stage.
    pub fn send_setup(channel: u64, streams: &Streams, argv: &[&str]) -> Result<()> {
        send_setup_env(channel, streams, argv, &Record::default())
    }

    /// As [`send_setup`], with an environment (Milestone 3.5 Part A).
    ///
    /// Kept as a separate entry point so every existing caller stays correct without
    /// edits — a spawn that has no environment to pass is not obliged to say so.
    pub fn send_setup_env(
        channel: u64,
        streams: &Streams,
        argv: &[&str],
        env: &Record,
    ) -> Result<()> {
        send_setup_full(channel, streams, None, argv, env)
    }

    /// The full form: streams, an optional **terminal**, `argv` and the environment.
    ///
    /// The terminal handle is **moved** like the streams, and rides *after* them in the
    /// handle array — see [`SetupPayload::terminal`] for why it is not one of them. A
    /// terminal emulator uses this to hand the shell it hosts the terminal it attached its
    /// backend to; every other caller passes `None` and gets what it always got.
    pub fn send_setup_full(
        channel: u64,
        streams: &Streams,
        terminal: Option<u64>,
        argv: &[&str],
        env: &Record,
    ) -> Result<()> {
        let owned: Vec<String> = argv.iter().map(|s| String::from(*s)).collect();
        let payload = SetupPayload {
            streams: streams.bitmap(),
            argv: owned,
            env: env.clone(),
            terminal: terminal.is_some(),
        }
        // `encode` enforces `IPC_PAYLOAD_SIZE` and reports `SetupTooLarge` — deliberately
        // not `SinkFull`, since a full channel is a transport condition a caller may retry
        // and this never will be.
        .encode()?;
        let mut handles = streams.ordered();
        if let Some(t) = terminal {
            handles.push(t);
        }
        let mut buf: Box<[u8; IPC_MSG_SIZE]> = Box::new([0u8; IPC_MSG_SIZE]);
        buf[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4]
            .copy_from_slice(&(payload.len() as u32).to_le_bytes());
        buf[OFF_HANDLE_COUNT] = handles.len() as u8;
        buf[OFF_PAYLOAD..OFF_PAYLOAD + payload.len()].copy_from_slice(&payload);
        // SAFETY: valid IPC_MSG_SIZE buffer + handle array; Block mode returns a
        // delivery PO to wait on (or a negative error).
        let r = unsafe {
            syscall6(
                SYS_CHANNEL_SEND,
                channel,
                buf.as_ptr() as u64,
                handles.as_ptr() as u64,
                handles.len() as u64,
                SENDMODE_BLOCK,
                u64::MAX,
            )
        };
        if r < 0 {
            return Err(map_err(r));
        }
        let po = r as u64;
        let wh = [po];
        let mut result = [0u8; 24];
        // SAFETY: valid handle + result out-buffers.
        let waited =
            unsafe { syscall4(SYS_WAIT, wh.as_ptr() as u64, 1, result.as_mut_ptr() as u64, u64::MAX) };
        // SAFETY: closing the PO we own.
        unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
        if waited < 1 {
            return Err(WireError::PeerClosed);
        }
        let status = i32::from_le_bytes([result[8], result[9], result[10], result[11]]);
        if status < 0 {
            return Err(map_err(status as i64));
        }
        Ok(())
    }

    /// The four bootstrap registers a process receives at `_start`, plus the opt-in
    /// setup message. Construct with [`bootstrap`].
    pub struct Bootstrap {
        notif: u64,
        namespace: u64,
        endpoint: u64,
        arg0: u64,
    }

    /// A Tier-1 stage's setup: its standard streams and `argv`.
    pub struct Setup {
        /// The stream handles (`stdin`/`stdout`/`stderr`), mapped from the message.
        pub streams: Streams,
        /// The command-line arguments (`argv[0]` is the program name).
        pub argv: Vec<String>,
        /// The environment this stage was given (empty if none).
        ///
        /// **Tier 0 has no environment, for the same reason it has no `argv`**: there is
        /// no setup message. That is not a gap — a process spawned without one was given
        /// nothing to start from, which is the whole meaning of the tier.
        pub env: Record,
        /// The terminal this stage was handed, if its parent gave it one.
        ///
        /// `None` is the ordinary case: a program finds its terminal by resolving
        /// `/dev/tty`. A program launched *by a terminal emulator* is handed one instead,
        /// because a namespace binding cannot name a particular window — see
        /// [`SetupPayload::terminal`].
        pub terminal: Option<u64>,
    }

    /// Wrap a process's four bootstrap registers (as received by `_start`).
    pub fn bootstrap(notif: u64, namespace: u64, endpoint: u64, arg0: u64) -> Bootstrap {
        Bootstrap {
            notif,
            namespace,
            endpoint,
            arg0,
        }
    }

    impl Bootstrap {
        /// The notification-channel handle.
        pub fn notif(&self) -> u64 {
            self.notif
        }
        /// The root-namespace handle.
        pub fn namespace(&self) -> u64 {
            self.namespace
        }
        /// The bootstrap endpoint handle (the setup channel for a Tier-1 stage).
        pub fn endpoint(&self) -> u64 {
            self.endpoint
        }
        /// The raw `arg0` descriptor word.
        pub fn arg0(&self) -> u64 {
            self.arg0
        }

        /// If `arg0` marks a pending setup message (Tier 1), receive it on the endpoint
        /// and parse the streams + `argv`. `None` for a Tier-0 process (no descriptor).
        pub fn setup(&self) -> Option<Result<Setup>> {
            if !setup_is_pending(self.arg0) {
                return None;
            }
            Some(self.recv_setup())
        }

        fn recv_setup(&self) -> Result<Setup> {
            let mut buf: Box<[u8; IPC_MSG_SIZE]> = Box::new([0u8; IPC_MSG_SIZE]);
            let mut handles = [0u64; IPC_HANDLE_MAX];
            let mut count: usize = 0;
            loop {
                // SAFETY: valid msg/handles/count out-params.
                let r = unsafe {
                    syscall4(
                        SYS_CHANNEL_RECV,
                        self.endpoint,
                        buf.as_mut_ptr() as u64,
                        handles.as_mut_ptr() as u64,
                        (&raw mut count) as u64,
                    )
                };
                if r == 0 {
                    break;
                }
                if r == E_WOULDBLOCK {
                    let wh = [self.endpoint];
                    let mut result = [0u8; 24];
                    // SAFETY: valid handle + result out-buffers.
                    let waited = unsafe {
                        syscall4(SYS_WAIT, wh.as_ptr() as u64, 1, result.as_mut_ptr() as u64, u64::MAX)
                    };
                    if waited < 1 {
                        return Err(WireError::PeerClosed);
                    }
                    continue;
                }
                return Err(map_err(r));
            }
            let n = u32::from_le_bytes([
                buf[OFF_PAYLOAD_LEN],
                buf[OFF_PAYLOAD_LEN + 1],
                buf[OFF_PAYLOAD_LEN + 2],
                buf[OFF_PAYLOAD_LEN + 3],
            ]) as usize;
            if n > IPC_PAYLOAD_SIZE {
                return Err(WireError::UnexpectedEof);
            }
            let payload = SetupPayload::decode(&buf[OFF_PAYLOAD..OFF_PAYLOAD + n])?;
            // The streams come first and the terminal, if any, after them — so the split
            // is by the bitmap's population count rather than by the handle count, which
            // includes both.
            let (streams, terminal) = super::split_handles(&payload, &handles[..count])?;
            Ok(Setup {
                streams,
                argv: payload.argv,
                env: payload.env,
                terminal,
            })
        }
    }
}

#[cfg(test)]
mod tests {


    /// The refusal is its own error, and it is asserted — an earlier pass added the
    /// variant with a justification and nothing that checked it, which a control caught by
    /// passing.
    #[test]
    fn a_claimed_terminal_with_no_handle_transferred_is_refused() {
        // **Not `Some(0)`.** The handle array a receiver holds is fixed and zero-initialised,
        // so being inside it proves nothing; only the transferred `count` does. A shell handed
        // `Some(0)` would run its entire REPL against handle 0 and every read would fail in a
        // way that looks like a dead terminal rather than a bad message.
        let claims = SetupPayload {
            streams: STREAM_STDOUT,
            argv: alloc::vec![],
            env: Record::default(),
            terminal: true,
        };
        assert!(split_handles(&claims, &[7]).is_err(), "one handle is the stream's");
        let (streams, terminal) = split_handles(&claims, &[7, 9]).expect("both transferred");
        assert_eq!(streams.stdout, Some(7));
        assert_eq!(terminal, Some(9), "the terminal follows the streams");
    }

    #[test]
    fn a_payload_with_no_terminal_ignores_a_handle_beyond_its_streams() {
        // The other direction: extra handles are not silently adopted as a terminal.
        let none = SetupPayload {
            streams: STREAM_STDIN,
            argv: alloc::vec![],
            env: Record::default(),
            terminal: false,
        };
        let (streams, terminal) = split_handles(&none, &[3, 4]).expect("ok");
        assert_eq!(streams.stdin, Some(3));
        assert_eq!(terminal, None);
    }

    #[test]
    fn a_terminal_survives_the_round_trip_and_its_absence_is_the_default() {
        // The flag and the handle are carried separately — the flag in the payload, the
        // handle in the message's transferred array — so a payload that claims a terminal
        // and one that does not have to be distinguishable here.
        let with = SetupPayload {
            streams: STREAM_STDOUT,
            argv: alloc::vec![String::from("nxsh")],
            env: Record::default(),
            terminal: true,
        };
        let got = SetupPayload::decode(&with.encode().unwrap()).unwrap();
        assert_eq!(got, with);
        assert!(got.terminal);

        let without = SetupPayload { terminal: false, ..with.clone() };
        let got = SetupPayload::decode(&without.encode().unwrap()).unwrap();
        assert!(!got.terminal, "a payload with no terminal decoded as having one");
        assert_ne!(with.encode().unwrap(), without.encode().unwrap());
    }

    #[test]
    fn a_sender_that_predates_the_terminal_field_still_decodes() {
        // **The extensibility the format claims, exercised in the direction that matters.**
        // `decode` reads by position and treats everything past `argv` as optional, so a
        // three-field message from before this field existed must still arrive — otherwise
        // adding a field is a flag day across every stage in the image.
        let schema = Schema::new()
            .field("streams", TypeTag::Int, TypeModifiers::NONE)
            .field("argv", TypeTag::List, TypeModifiers::NONE)
            .field("env", TypeTag::Record, TypeModifiers::NONE);
        let old = Record {
            schema,
            values: alloc::vec![
                Value::Int(STREAM_STDIN as i64),
                Value::List(Arc::from(alloc::vec![Value::Str(String::from("ls"))])),
                Value::Record(Arc::new(Record::default())),
            ],
        };
        let mut buf = alloc::vec::Vec::new();
        write_value(&mut buf, &Value::Record(Arc::new(old))).unwrap();

        let got = SetupPayload::decode(&buf).expect("a three-field message still decodes");
        assert_eq!(got.streams, STREAM_STDIN);
        assert_eq!(got.argv, alloc::vec![String::from("ls")]);
        assert!(!got.terminal, "an absent field is not a terminal");
    }

    #[test]
    fn an_oversized_environment_is_refused_by_name() {
        let mut schema = Schema::new();
        let mut values = Vec::new();
        for i in 0..400 {
            let name = alloc::format!("VAR_{i}");
            schema = schema.field(&name, TypeTag::String, TypeModifiers::NONE);
            values.push(Value::Str(String::from("0123456789012345")));
        }
        let p = SetupPayload {
            terminal: false,
            streams: 0,
            argv: alloc::vec![String::from("x")],
            env: Record { schema, values },
        };
        match p.encode() {
            Err(WireError::SetupTooLarge) => {}
            Err(e) => panic!("expected SetupTooLarge, got {e:?}"),
            Ok(b) => panic!("expected a refusal, encoded {} bytes", b.len()),
        }
    }

    // --- env on the wire (Milestone 3.5 Part A) -----------------------------

    fn env_record() -> Record {
        let schema = Schema::new()
            .field("PWD", TypeTag::String, TypeModifiers::NONE)
            .field("PATH", TypeTag::List, TypeModifiers::NONE);
        let path: Vec<Value> = alloc::vec![
            Value::Str(String::from("/bin")),
            Value::Str(String::from("/initramfs/sbin")),
        ];
        Record {
            schema,
            values: alloc::vec![
                Value::Str(String::from("/home/alice")),
                Value::List(Arc::from(path)),
            ],
        }
    }

    #[test]
    fn env_round_trips_as_a_typed_record() {
        let p = SetupPayload {
            terminal: false,
            streams: 0b011,
            argv: alloc::vec![String::from("list"), String::from("--long")],
            env: env_record(),
        };
        let back = SetupPayload::decode(&p.encode().expect("encodes")).expect("decodes");
        assert_eq!(back.streams, p.streams);
        assert_eq!(back.argv, p.argv);
        assert_eq!(back.env, p.env);
        // The point of typing it: `PATH` comes back as a list, not a string to split.
        let path = back.env.values[1].as_list().expect("PATH is a List");
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].as_str(), Some("/bin"));
    }

    /// A sender with nothing to pass produces an empty env, not a special case.
    #[test]
    fn an_absent_environment_is_empty_not_an_error() {
        let p = SetupPayload {
            terminal: false,
            streams: 0,
            argv: alloc::vec![String::from("x")],
            env: Record::default(),
        };
        let back = SetupPayload::decode(&p.encode().expect("encodes")).expect("decodes");
        assert!(back.env.values.is_empty());
    }

    /// **The compatibility claim, asserted rather than reasoned about.**
    ///
    /// The format extends in one direction only: appending is safe because `decode` reads
    /// by position and treats anything past `argv` as optional. This builds a payload in
    /// the *old* two-field shape by hand and decodes it with the current reader — which is
    /// what every stage in the image is doing until it is rebuilt.
    #[test]
    fn an_old_two_field_payload_still_decodes() {
        let schema = Schema::new()
            .field("streams", TypeTag::Int, TypeModifiers::NONE)
            .field("argv", TypeTag::List, TypeModifiers::NONE);
        let argv: Vec<Value> = alloc::vec![Value::Str(String::from("old"))];
        let old = Record {
            schema,
            values: alloc::vec![Value::Int(0b101), Value::List(Arc::from(argv))],
        };
        let mut buf = Vec::new();
        write_value(&mut buf, &Value::Record(Arc::new(old))).expect("encodes");

        let back = SetupPayload::decode(&buf).expect("an old payload must still decode");
        assert_eq!(back.streams, 0b101);
        assert_eq!(back.argv, alloc::vec![String::from("old")]);
        assert!(back.env.values.is_empty(), "no env is empty, not garbage");
    }

    /// …and the converse: a *truncated* payload is still refused, so the tolerance above
    /// is "later fields are optional", not "any record will do".
    #[test]
    fn a_payload_missing_argv_is_still_rejected() {
        let schema = Schema::new().field("streams", TypeTag::Int, TypeModifiers::NONE);
        let short = Record { schema, values: alloc::vec![Value::Int(1)] };
        let mut buf = Vec::new();
        write_value(&mut buf, &Value::Record(Arc::new(short))).expect("encodes");
        assert!(SetupPayload::decode(&buf).is_err());
    }
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
            let p = SetupPayload {
            terminal: false, streams: STREAM_STDOUT | STREAM_STDERR, argv, env: Record::default() };
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
