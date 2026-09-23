//! `Views` (`op = 0x0Exx`) — the view broker, served at `/svc/views` and, per session, at
//! `/dev/views`. See `docs/spec/rsproto-views-ops.md` and `docs/planning/administration.md`
//! § Part A.
//!
//! **Two kinds of channel, told apart by the path they were resolved through.** A login
//! supervisor resolves `/svc/views/session` and gets a *supervisor* channel, on which it opens a
//! session for the principal it authenticated and closes it when the session ends. A process in
//! the session resolves `/dev/views` — the same forwarding endpoint, bound with the subtree base
//! `/s/<session>` — and gets a *client* channel the broker has already tagged with that session.
//! Nothing a client sends names a principal: the path it came through is the identity.
//!
//! **A request is two steps when the policy wants a password.** `Request` names the view and the
//! program and carries the caller's namespace, streams and terminal; the reply is `Started`,
//! `Denied`, or `NeedPassword`, and after the last the client sends `Password` and gets `Started`
//! or `Denied` back. The client reads the password itself, on the terminal its shell handed it;
//! the broker never touches a terminal. When the program exits the broker sends `Exited`,
//! unsolicited.
//!
//! Bodies are little-endian and byte-serialised into a caller buffer, like every category here.

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

/// Supervisor → broker: open a session for the principal in the body (its bytes). The reply
/// body is the new session's id, a `u64`. Ids increase and are never reused within a boot.
pub const OP_VIEWS_OPEN_SESSION: u16 = 0x0E00;
/// Supervisor → broker: the session whose id is the body (a `u64`) has ended. Empty reply.
pub const OP_VIEWS_CLOSE_SESSION: u16 = 0x0E01;
/// Client → broker: run a program in a view. Body [`build_request`]; handles as
/// [`ViewRequest::handles`] says. Reply: an [`Outcome`].
pub const OP_VIEWS_REQUEST: u16 = 0x0E02;
/// Client → broker: the password a `NeedPassword` asked for (its bytes). Reply: an [`Outcome`].
pub const OP_VIEWS_PASSWORD: u16 = 0x0E03;
/// Broker → client, unsolicited, `request_id` 0: the program exited. Body [`build_exited`].
pub const OP_VIEWS_EXITED: u16 = 0x0E04;
/// Client → broker: ask the program to stop — the shell asked the client to. Empty body and
/// reply. A request, like every stop in this system: the program decides.
pub const OP_VIEWS_STOP: u16 = 0x0E05;
/// Client → broker: which views may this session's principal use? Reply: rows, [`push_row`].
pub const OP_VIEWS_LIST: u16 = 0x0E06;
/// Client → broker: is this text a valid policy? Body: the text. Reply: an [`Outcome`] —
/// `Started` meaning "valid", `Denied` with the reason otherwise. No authority needed: judging a
/// file installs nothing.
pub const OP_VIEWS_CHECK: u16 = 0x0E07;

/// The broker's answer to a `Request`, a `Password` or a `Check`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The program is running (or, for `Check`, the policy is valid).
    Started,
    /// The rule wants a password; send one with `OP_VIEWS_PASSWORD`.
    NeedPassword,
    /// Refused. `retry` is whether another `Password` on the same request will be heard.
    Denied { retry: bool },
}

const OUTCOME_STARTED: u8 = 0;
const OUTCOME_NEED_PASSWORD: u8 = 1;
const OUTCOME_DENIED: u8 = 2;

/// Fixed prefix of an outcome body: kind, retry, reason length.
pub const OUTCOME_PREFIX_LEN: usize = 4;

/// Write an outcome body — the kind, and for a denial whether to retry — followed by `reason`,
/// which a client shows as written. Returns its length, or `None` if `out` is too small or the
/// reason longer than `u16::MAX`.
pub fn build_outcome(out: &mut [u8], outcome: Outcome, reason: &[u8]) -> Option<usize> {
    if reason.len() > u16::MAX as usize || out.len() < OUTCOME_PREFIX_LEN + reason.len() {
        return None;
    }
    let (kind, retry) = match outcome {
        Outcome::Started => (OUTCOME_STARTED, false),
        Outcome::NeedPassword => (OUTCOME_NEED_PASSWORD, false),
        Outcome::Denied { retry } => (OUTCOME_DENIED, retry),
    };
    out[0] = kind;
    out[1] = retry as u8;
    put_u16(out, 2, reason.len() as u16);
    out[OUTCOME_PREFIX_LEN..OUTCOME_PREFIX_LEN + reason.len()].copy_from_slice(reason);
    Some(OUTCOME_PREFIX_LEN + reason.len())
}

/// Parse an outcome body into the outcome and its reason. `None` for an unknown kind, a retry
/// byte that is neither 0 nor 1, or a reason length that runs past the body.
pub fn parse_outcome(body: &[u8]) -> Option<(Outcome, &[u8])> {
    if body.len() < OUTCOME_PREFIX_LEN || body[1] > 1 {
        return None;
    }
    let len = get_u16(body, 2) as usize;
    let reason = body.get(OUTCOME_PREFIX_LEN..OUTCOME_PREFIX_LEN + len)?;
    let outcome = match body[0] {
        OUTCOME_STARTED => Outcome::Started,
        OUTCOME_NEED_PASSWORD => Outcome::NeedPassword,
        OUTCOME_DENIED => Outcome::Denied { retry: body[1] == 1 },
        _ => return None,
    };
    Some((outcome, reason))
}

// --- Request -----------------------------------------------------------------

/// `handles` bit: a `stdin` follows the namespace.
pub const REQ_STDIN: u8 = 1 << 0;
/// `handles` bit: a `stdout` follows.
pub const REQ_STDOUT: u8 = 1 << 1;
/// `handles` bit: a `stderr` follows.
pub const REQ_STDERR: u8 = 1 << 2;
/// `handles` bit: a terminal follows the streams.
pub const REQ_TERMINAL: u8 = 1 << 3;
const REQ_KNOWN: u8 = REQ_STDIN | REQ_STDOUT | REQ_STDERR | REQ_TERMINAL;

/// A parsed `Request`, borrowing the body.
#[derive(Copy, Clone, Debug)]
pub struct ViewRequest<'a> {
    /// Which handles ride with the message after the namespace, which always comes first:
    /// [`REQ_STDIN`], [`REQ_STDOUT`], [`REQ_STDERR`] and [`REQ_TERMINAL`], in that order.
    pub handles: u8,
    /// The view, by name — a profile in the policy.
    pub view: &'a [u8],
    /// The program, a bare name the broker resolves under its own `/bin`.
    pub program: &'a [u8],
    argc: u16,
    args: &'a [u8],
    /// The caller's environment, a TSM1 record, opaque here. The broker passes it on.
    pub env: &'a [u8],
}

impl<'a> ViewRequest<'a> {
    /// The program's arguments, not including its name.
    pub fn args(&self) -> Args<'a> {
        Args { left: self.argc, rest: self.args }
    }

    /// How many handles the message should carry: the namespace, and one per bit set.
    pub fn handle_count(&self) -> usize {
        1 + self.handles.count_ones() as usize
    }
}

/// The arguments of a [`ViewRequest`], each a byte string.
#[derive(Copy, Clone, Debug)]
pub struct Args<'a> {
    left: u16,
    rest: &'a [u8],
}

impl<'a> Iterator for Args<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 {
            return None;
        }
        // `parse_request` walked every argument before handing this out, so each length
        // prefix is known to fit; a short read here would be a bug in that walk.
        let len = get_u16(self.rest, 0) as usize;
        let arg = &self.rest[2..2 + len];
        self.rest = &self.rest[2 + len..];
        self.left -= 1;
        Some(arg)
    }
}

/// Write a `Request` body. `handles` says which handles the caller will attach after the
/// namespace. Returns the length, or `None` if `out` is too small, a field is too long for its
/// length prefix, or `handles` sets an unknown bit.
pub fn build_request(
    out: &mut [u8],
    handles: u8,
    view: &[u8],
    program: &[u8],
    args: &[&[u8]],
    env: &[u8],
) -> Option<usize> {
    if handles & !REQ_KNOWN != 0 || args.len() > u16::MAX as usize {
        return None;
    }
    let mut at = 0usize;
    let put_bytes = |out: &mut [u8], at: &mut usize, b: &[u8]| -> Option<()> {
        if b.len() > u16::MAX as usize || out.len() < *at + 2 + b.len() {
            return None;
        }
        put_u16(out, *at, b.len() as u16);
        out[*at + 2..*at + 2 + b.len()].copy_from_slice(b);
        *at += 2 + b.len();
        Some(())
    };
    if out.is_empty() {
        return None;
    }
    out[0] = handles;
    at += 1;
    put_bytes(out, &mut at, view)?;
    put_bytes(out, &mut at, program)?;
    if out.len() < at + 2 {
        return None;
    }
    put_u16(out, at, args.len() as u16);
    at += 2;
    for a in args {
        put_bytes(out, &mut at, a)?;
    }
    if env.len() > u32::MAX as usize || out.len() < at + 4 + env.len() {
        return None;
    }
    put_u32(out, at, env.len() as u32);
    out[at + 4..at + 4 + env.len()].copy_from_slice(env);
    Some(at + 4 + env.len())
}

/// Parse a `Request` body. `None` for an unknown handle bit, any length that runs past the
/// body, an empty view or program, or bytes left over at the end.
pub fn parse_request(body: &[u8]) -> Option<ViewRequest<'_>> {
    let handles = *body.first()?;
    if handles & !REQ_KNOWN != 0 {
        return None;
    }
    let mut at = 1usize;
    let take = |at: &mut usize| -> Option<&[u8]> {
        let len = get_u16(body.get(*at..*at + 2)?, 0) as usize;
        let b = body.get(*at + 2..*at + 2 + len)?;
        *at += 2 + len;
        Some(b)
    };
    let view = take(&mut at)?;
    let program = take(&mut at)?;
    if view.is_empty() || program.is_empty() {
        return None;
    }
    let argc = get_u16(body.get(at..at + 2)?, 0);
    at += 2;
    let args_start = at;
    for _ in 0..argc {
        take(&mut at)?;
    }
    let args = &body[args_start..at];
    let env_len = get_u32(body.get(at..at + 4)?, 0) as usize;
    let env = body.get(at + 4..at + 4 + env_len)?;
    if at + 4 + env_len != body.len() {
        return None;
    }
    Some(ViewRequest { handles, view, program, argc, args, env })
}

// --- Sessions ----------------------------------------------------------------

/// Write a session id — `OpenSession`'s reply, `CloseSession`'s request. Returns 8, or `None`
/// if `out` is too small.
pub fn build_session_id(out: &mut [u8], id: u64) -> Option<usize> {
    if out.len() < 8 {
        return None;
    }
    put_u64(out, 0, id);
    Some(8)
}

/// Parse a session id. `None` unless the body is exactly eight bytes.
pub fn parse_session_id(body: &[u8]) -> Option<u64> {
    (body.len() == 8).then(|| get_u64(body, 0))
}

// --- Exited ------------------------------------------------------------------

/// Length of an `Exited` body: the code, and whether the program crashed rather than exiting.
pub const EXITED_LEN: usize = 5;

/// Write an `Exited` body. Returns [`EXITED_LEN`], or `None` if `out` is too small.
pub fn build_exited(out: &mut [u8], code: i32, crashed: bool) -> Option<usize> {
    if out.len() < EXITED_LEN {
        return None;
    }
    put_u32(out, 0, code as u32);
    out[4] = crashed as u8;
    Some(EXITED_LEN)
}

/// Parse an `Exited` body into `(code, crashed)`. `None` unless it is exactly [`EXITED_LEN`]
/// bytes with a crashed byte of 0 or 1.
pub fn parse_exited(body: &[u8]) -> Option<(i32, bool)> {
    if body.len() != EXITED_LEN || body[4] > 1 {
        return None;
    }
    Some((get_u32(body, 0) as i32, body[4] == 1))
}

// --- List --------------------------------------------------------------------

/// One row of `List`'s reply: a view this principal may use, the programs it may run there,
/// and whether a password is asked for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Row<'a> {
    /// The view's name.
    pub view: &'a [u8],
    /// The programs, space-separated as the policy lists them, or `*` for any.
    pub run: &'a [u8],
    /// Whether the rule asks for a password.
    pub password: bool,
}

/// Append `row` to a `List` reply being built in `out` at `*at`, counting it in the `u16` at
/// offset 0 (which the caller zeroes before the first row, by starting `*at` at 2). `None` if
/// it does not fit.
pub fn push_row(out: &mut [u8], at: &mut usize, row: &Row<'_>) -> Option<()> {
    if *at < 2 || out.len() < 2 {
        return None;
    }
    let need = 2 + row.view.len() + 2 + row.run.len() + 1;
    if row.view.len() > u16::MAX as usize
        || row.run.len() > u16::MAX as usize
        || out.len() < *at + need
    {
        return None;
    }
    let count = get_u16(out, 0).checked_add(1)?;
    put_u16(out, *at, row.view.len() as u16);
    out[*at + 2..*at + 2 + row.view.len()].copy_from_slice(row.view);
    let r = *at + 2 + row.view.len();
    put_u16(out, r, row.run.len() as u16);
    out[r + 2..r + 2 + row.run.len()].copy_from_slice(row.run);
    out[r + 2 + row.run.len()] = row.password as u8;
    *at += need;
    put_u16(out, 0, count);
    Some(())
}

/// Parse a `List` reply into its rows. `None` if a row runs past the body, a password byte is
/// neither 0 nor 1, or bytes are left over.
pub fn parse_rows(body: &[u8], mut each: impl FnMut(Row<'_>)) -> Option<usize> {
    let n = get_u16(body.get(..2)?, 0) as usize;
    let mut at = 2usize;
    for _ in 0..n {
        let vl = get_u16(body.get(at..at + 2)?, 0) as usize;
        let view = body.get(at + 2..at + 2 + vl)?;
        let r = at + 2 + vl;
        let rl = get_u16(body.get(r..r + 2)?, 0) as usize;
        let run = body.get(r + 2..r + 2 + rl)?;
        let p = *body.get(r + 2 + rl)?;
        if p > 1 {
            return None;
        }
        each(Row { view, run, password: p == 1 });
        at = r + 2 + rl + 1;
    }
    (at == body.len()).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_with_its_arguments_and_environment() {
        let mut buf = [0u8; 256];
        let args: [&[u8]; 2] = [b"--list", b""];
        let handles = REQ_STDOUT | REQ_TERMINAL;
        let n = build_request(&mut buf, handles, b"admin", b"nxinstall", &args, b"ENV").unwrap();
        let r = parse_request(&buf[..n]).unwrap();
        assert_eq!(r.handles, REQ_STDOUT | REQ_TERMINAL);
        assert_eq!(r.handle_count(), 3, "the namespace plus two");
        assert_eq!((r.view, r.program, r.env), (&b"admin"[..], &b"nxinstall"[..], &b"ENV"[..]));
        let got: Vec<&[u8]> = r.args().collect();
        assert_eq!(got, [&b"--list"[..], &b""[..]], "an empty argument is still an argument");
    }

    /// **The reader, tested with bytes a correct writer never produces** — the round trip above
    /// only proves the two agree with each other.
    #[test]
    fn a_request_the_writer_could_not_have_made_is_refused() {
        let mut buf = [0u8; 64];
        let n = build_request(&mut buf, 0, b"admin", b"disk", &[b"a"], b"").unwrap();
        let good = &buf[..n];
        assert!(parse_request(good).is_some(), "precondition");

        let mut bad = good.to_vec();
        bad[0] = 1 << 4; // an unknown handle bit
        assert!(parse_request(&bad).is_none(), "unknown handle bit");

        assert!(parse_request(&good[..n - 1]).is_none(), "truncated environment length");
        let mut long = good.to_vec();
        long.push(0);
        assert!(parse_request(&long).is_none(), "trailing bytes");

        // An argument count larger than the arguments there are.
        let argc_at = 1 + 2 + 5 + 2 + 4;
        let mut more = good.to_vec();
        more[argc_at] = 2;
        assert!(parse_request(&more).is_none(), "argc past the body");

        // An empty view or program names nothing.
        let n = build_request(&mut buf, 0, b"", b"disk", &[], b"").unwrap();
        assert!(parse_request(&buf[..n]).is_none(), "empty view");
        let n = build_request(&mut buf, 0, b"admin", b"", &[], b"").unwrap();
        assert!(parse_request(&buf[..n]).is_none(), "empty program");
        assert!(parse_request(&[]).is_none());
    }

    #[test]
    fn an_outcome_round_trips_and_a_malformed_one_is_refused() {
        let mut buf = [0u8; 32];
        let all = [
            Outcome::Started,
            Outcome::NeedPassword,
            Outcome::Denied { retry: true },
            Outcome::Denied { retry: false },
        ];
        for o in all {
            let n = build_outcome(&mut buf, o, b"why").unwrap();
            assert_eq!(parse_outcome(&buf[..n]), Some((o, &b"why"[..])));
        }
        let n = build_outcome(&mut buf, Outcome::Started, b"").unwrap();
        let mut bad = buf[..n].to_vec();
        bad[0] = 3;
        assert!(parse_outcome(&bad).is_none(), "unknown kind");
        bad[0] = 0;
        bad[1] = 2;
        assert!(parse_outcome(&bad).is_none(), "retry byte that is not a bool");
        bad[1] = 0;
        bad[2] = 9;
        assert!(parse_outcome(&bad).is_none(), "reason length past the body");
    }

    #[test]
    fn exited_and_session_ids_round_trip_and_refuse_the_wrong_length() {
        let mut buf = [0u8; 8];
        let n = build_exited(&mut buf, -3, true).unwrap();
        assert_eq!(parse_exited(&buf[..n]), Some((-3, true)));
        assert!(parse_exited(&buf[..n - 1]).is_none());
        let mut bad = buf;
        bad[4] = 2;
        assert!(parse_exited(&bad[..n]).is_none(), "crashed byte that is not a bool");

        let n = build_session_id(&mut buf, 41).unwrap();
        assert_eq!(parse_session_id(&buf[..n]), Some(41));
        assert!(parse_session_id(&buf[..7]).is_none());
    }

    #[test]
    fn rows_round_trip_and_a_count_past_the_rows_is_refused() {
        let mut buf = [0u8; 128];
        let mut at = 2;
        push_row(&mut buf, &mut at, &Row { view: b"admin", run: b"*", password: true }).unwrap();
        push_row(&mut buf, &mut at, &Row { view: b"power", run: b"shutdown", password: false })
            .unwrap();
        let mut seen = Vec::new();
        let n =
            parse_rows(&buf[..at], |r| seen.push((r.view.to_vec(), r.run.to_vec(), r.password)));
        assert_eq!(n, Some(2));
        assert_eq!(seen[1], (b"power".to_vec(), b"shutdown".to_vec(), false));

        let mut bad = buf[..at].to_vec();
        bad[0] = 3;
        assert!(parse_rows(&bad, |_| {}).is_none(), "a count past the rows");
        assert!(parse_rows(&buf[..at - 1], |_| {}).is_none(), "a truncated last row");
    }
}
