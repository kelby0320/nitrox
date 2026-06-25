//! `Meta` category (`op = 0x00xx`) bodies: Hello, Goodbye, QueryCaps, Ping,
//! Ready. See `docs/spec/rsproto-wire-format.md` § "Version negotiation".

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

// --- Hello (version negotiation) --------------------------------------------

/// `HelloRequest` wire length.
pub const HELLO_REQUEST_LEN: usize = 8;
/// `HelloReply` wire length.
pub const HELLO_REPLY_LEN: usize = 10;

/// A parsed `HelloRequest` (the client's acceptable version range).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HelloRequest {
    pub client_min_version: u16,
    pub client_max_version: u16,
    pub flags: u32,
}

/// Write a `HelloRequest` body; returns its length.
pub fn hello_request(out: &mut [u8], min: u16, max: u16, flags: u32) -> Option<usize> {
    if out.len() < HELLO_REQUEST_LEN {
        return None;
    }
    put_u16(out, 0, min);
    put_u16(out, 2, max);
    put_u32(out, 4, flags);
    Some(HELLO_REQUEST_LEN)
}

/// Parse a `HelloRequest` body.
pub fn parse_hello_request(body: &[u8]) -> Option<HelloRequest> {
    if body.len() < HELLO_REQUEST_LEN {
        return None;
    }
    Some(HelloRequest {
        client_min_version: get_u16(body, 0),
        client_max_version: get_u16(body, 2),
        flags: get_u32(body, 4),
    })
}

/// A parsed `HelloReply` (the agreed version + the server's capabilities).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HelloReply {
    pub agreed_version: u16,
    pub server_caps: u32,
    pub server_flags: u32,
}

/// Write a `HelloReply` body; returns its length.
pub fn hello_reply(out: &mut [u8], agreed: u16, caps: u32, flags: u32) -> Option<usize> {
    if out.len() < HELLO_REPLY_LEN {
        return None;
    }
    put_u16(out, 0, agreed);
    put_u32(out, 2, caps);
    put_u32(out, 6, flags);
    Some(HELLO_REPLY_LEN)
}

/// Parse a `HelloReply` body.
pub fn parse_hello_reply(body: &[u8]) -> Option<HelloReply> {
    if body.len() < HELLO_REPLY_LEN {
        return None;
    }
    Some(HelloReply {
        agreed_version: get_u16(body, 0),
        server_caps: get_u32(body, 2),
        server_flags: get_u32(body, 6),
    })
}

// --- Ping (liveness) --------------------------------------------------------

/// Ping request/reply body length (an opaque nonce).
pub const PING_LEN: usize = 8;

/// Write a Ping nonce body; returns its length.
pub fn ping(out: &mut [u8], nonce: u64) -> Option<usize> {
    if out.len() < PING_LEN {
        return None;
    }
    put_u64(out, 0, nonce);
    Some(PING_LEN)
}

/// Parse a Ping nonce body.
pub fn parse_ping(body: &[u8]) -> Option<u64> {
    if body.len() < PING_LEN {
        return None;
    }
    Some(get_u64(body, 0))
}

// --- Ready (startup signal on the control channel) --------------------------

/// Fixed prefix of a `ReadyMessage` (before the server name).
pub const READY_PREFIX_LEN: usize = 4;

/// Write a `ReadyMessage` body (`server_name` is informational; the endpoint
/// handle rides in `IpcMsg.handles[0]`); returns its length.
pub fn ready(out: &mut [u8], server_name: &[u8]) -> Option<usize> {
    if server_name.len() > u16::MAX as usize {
        return None;
    }
    let total = READY_PREFIX_LEN + server_name.len();
    if out.len() < total {
        return None;
    }
    put_u16(out, 0, server_name.len() as u16);
    put_u16(out, 2, 0);
    out[READY_PREFIX_LEN..total].copy_from_slice(server_name);
    Some(total)
}

/// Parse a `ReadyMessage` body, returning the server-name bytes.
pub fn parse_ready(body: &[u8]) -> Option<&[u8]> {
    if body.len() < READY_PREFIX_LEN {
        return None;
    }
    let name_len = get_u16(body, 0) as usize;
    let end = READY_PREFIX_LEN.checked_add(name_len)?;
    if body.len() < end {
        return None;
    }
    Some(&body[READY_PREFIX_LEN..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_round_trips() {
        let mut buf = [0u8; 16];
        let n = hello_request(&mut buf, 1, 3, 0).unwrap();
        assert_eq!(n, HELLO_REQUEST_LEN);
        let r = parse_hello_request(&buf[..n]).unwrap();
        assert_eq!(r, HelloRequest { client_min_version: 1, client_max_version: 3, flags: 0 });

        let n = hello_reply(&mut buf, 2, 0b101, 0).unwrap();
        assert_eq!(n, HELLO_REPLY_LEN);
        let r = parse_hello_reply(&buf[..n]).unwrap();
        assert_eq!(r, HelloReply { agreed_version: 2, server_caps: 0b101, server_flags: 0 });
    }

    #[test]
    fn ping_echoes() {
        let mut buf = [0u8; 8];
        ping(&mut buf, 0xCAFE_F00D).unwrap();
        assert_eq!(parse_ping(&buf), Some(0xCAFE_F00D));
    }

    #[test]
    fn ready_carries_name() {
        let mut buf = [0u8; 64];
        let n = ready(&mut buf, b"fs-server-ext4").unwrap();
        assert_eq!(parse_ready(&buf[..n]), Some(&b"fs-server-ext4"[..]));
        // Empty name is valid.
        let n = ready(&mut buf, b"").unwrap();
        assert_eq!(parse_ready(&buf[..n]), Some(&b""[..]));
    }
}
