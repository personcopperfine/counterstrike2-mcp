//! Minimal Source RCON client (TCP), used for the dedicated server's console (`-usercon` plus
//! `rcon_password`; workflows/05 step 1).
//!
//! Wire format (Valve "Source RCON protocol"): every packet is little-endian `i32 size`, `i32 id`,
//! `i32 type`, a NUL-terminated ASCII body and one more NUL. `size` counts everything after itself
//! (`4 + 4 + body + 2`).
//!
//! What a live CS2 dedicated server does (observed; see client-mcp/workflows/05-multiplayer.md):
//! * auth: the reply is ONE `type 2` packet (AUTH_RESPONSE), with no empty `type 0` before it.
//!   `id == request id` is accepted, `id == -1` is a wrong password.
//! * exec: a command's reply can be one packet or several. The empty `type 0` sentinel that the
//!   classic protocol uses to mark the end is NOT answered by CS2. So the client sends the sentinel,
//!   ends early if the server answers it, and otherwise ends after a quiet period once the first
//!   reply packet has arrived.
//!
//! Reads go through an internal buffer and every read is cancel-safe, so a timeout between
//! packets never loses bytes. This module has no game knowledge. Tests run against a fake server
//! (`cs2_mcp_common::mock::rcon`).

use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const SERVERDATA_RESPONSE_VALUE: i32 = 0;
/// Request type for a command (`EXECCOMMAND`).
pub const SERVERDATA_EXECCOMMAND: i32 = 2;
/// Response type for the auth reply (`AUTH_RESPONSE`); same number as `EXECCOMMAND`.
pub const SERVERDATA_AUTH_RESPONSE: i32 = 2;
pub const SERVERDATA_AUTH: i32 = 3;
/// `id` the server uses to reject a password (or a command sent before auth).
pub const REJECTED_ID: i32 = -1;
/// Smallest legal `size` (id + type + two NULs, empty body).
pub const MIN_SIZE: i32 = 10;
/// Largest packet accepted from the server. Source splits replies at 4096 body bytes; this is a
/// memory cap for a misbehaving peer, not a protocol number.
pub const MAX_PACKET_SIZE: i32 = 64 * 1024;
/// Largest accumulated reply to one command.
pub const MAX_REPLY_BYTES: usize = 1024 * 1024;
/// After the first reply packet, a command's reply is complete once this much time passes with no
/// further packet (CS2 does not answer the sentinel).
pub const REPLY_QUIET: Duration = Duration::from_millis(300);
/// Largest command the protocol carries: 4096 bytes of body, minus the 10 bytes of framing and the
/// NULs (the server rejects longer requests).
pub const MAX_COMMAND_BYTES: usize = 4086;

#[derive(Debug, Error)]
pub enum RconError {
    #[error("rcon I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("rcon did not answer within {0:?}")]
    Timeout(Duration),
    #[error("rcon authentication failed (wrong rcon_password)")]
    AuthFailed,
    #[error("rcon command refused: not authenticated")]
    NotAuthenticated,
    #[error("rcon protocol error: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub id: i32,
    pub ty: i32,
    pub body: String,
}

/// One packet on the wire.
pub fn encode(id: i32, ty: i32, body: &str) -> Vec<u8> {
    let size = (4 + 4 + body.len() + 2) as i32;
    let mut out = Vec::with_capacity(4 + size as usize);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

/// Parse one packet from the front of `buf`. Returns the packet and the number of bytes used.
/// `Ok(None)` when `buf` does not hold a whole packet yet.
pub fn decode(buf: &[u8]) -> Result<Option<(Packet, usize)>, RconError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let size = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    check_size(size)?;
    let total = 4 + size as usize;
    if buf.len() < total {
        return Ok(None);
    }
    Ok(Some((parse_body(&buf[4..total], size)?, total)))
}

fn check_size(size: i32) -> Result<(), RconError> {
    if !(MIN_SIZE..=MAX_PACKET_SIZE).contains(&size) {
        return Err(RconError::Protocol(format!(
            "packet size {size} out of range"
        )));
    }
    Ok(())
}

fn parse_body(payload: &[u8], size: i32) -> Result<Packet, RconError> {
    let id = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let ty = i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let body_end = size as usize - 2;
    if payload[body_end] != 0 || payload[body_end + 1] != 0 {
        return Err(RconError::Protocol(
            "packet is not NUL-terminated twice".into(),
        ));
    }
    Ok(Packet {
        id,
        ty,
        body: String::from_utf8_lossy(&payload[8..body_end]).into_owned(),
    })
}

/// Read exactly one packet from a stream. Not cancel-safe: for the fake server, which never times out.
pub async fn read_packet<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Packet, RconError> {
    let mut size_buf = [0u8; 4];
    r.read_exact(&mut size_buf).await?;
    let size = i32::from_le_bytes(size_buf);
    check_size(size)?;
    let mut payload = vec![0u8; size as usize];
    r.read_exact(&mut payload).await?;
    parse_body(&payload, size)
}

/// An RCON connection over any byte stream (TCP in production).
pub struct RconClient<S = TcpStream> {
    stream: S,
    next_id: i32,
    /// bytes read from the stream but not yet parsed into packets
    buf: Vec<u8>,
}

impl RconClient<TcpStream> {
    /// Open the TCP connection (no auth yet). `addr` is `host:port`.
    pub async fn connect(addr: &str, timeout: Duration) -> Result<Self, RconError> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| RconError::Timeout(timeout))??;
        Ok(Self::from_stream(stream))
    }
}

impl<S: AsyncReadExt + AsyncWriteExt + Unpin> RconClient<S> {
    pub fn from_stream(stream: S) -> Self {
        RconClient {
            stream,
            next_id: 1,
            buf: Vec::new(),
        }
    }

    fn alloc_id(&mut self) -> i32 {
        let id = self.next_id;
        // ids must stay positive: -1 is the rejection marker
        self.next_id = if id >= i32::MAX - 2 { 1 } else { id + 1 };
        id
    }

    async fn send(&mut self, id: i32, ty: i32, body: &str) -> Result<(), RconError> {
        self.stream.write_all(&encode(id, ty, body)).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// The next whole packet. Cancel-safe: a dropped future loses no bytes (they stay in `buf`).
    async fn next_packet(&mut self) -> Result<Packet, RconError> {
        loop {
            if let Some((p, used)) = decode(&self.buf)? {
                self.buf.drain(..used);
                return Ok(p);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(RconError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed the rcon connection",
                )));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Send the password and wait for the verdict. Other packets before the AUTH_RESPONSE (an empty
    /// `type 0` on classic servers) are skipped; a lone AUTH_RESPONSE (CS2) is enough.
    pub async fn auth(&mut self, password: &str, timeout: Duration) -> Result<(), RconError> {
        let id = self.alloc_id();
        tokio::time::timeout(timeout, async {
            self.send(id, SERVERDATA_AUTH, password).await?;
            loop {
                let p = self.next_packet().await?;
                if p.ty != SERVERDATA_AUTH_RESPONSE {
                    continue;
                }
                return match p.id {
                    REJECTED_ID => Err(RconError::AuthFailed),
                    got if got == id => Ok(()),
                    got => Err(RconError::Protocol(format!(
                        "auth reply id {got}, expected {id}"
                    ))),
                };
            }
        })
        .await
        .map_err(|_| RconError::Timeout(timeout))?
    }

    /// Run one command and return its reply text (all reply packets, concatenated).
    ///
    /// Ends at the sentinel's reply when the server sends one; otherwise, once a reply packet has
    /// arrived, after [`REPLY_QUIET`] with nothing more. `timeout` bounds the whole call: no reply
    /// packet at all within it is `Timeout`.
    pub async fn exec(&mut self, command: &str, timeout: Duration) -> Result<String, RconError> {
        if command.len() > MAX_COMMAND_BYTES {
            return Err(RconError::Protocol(format!(
                "command is {} bytes; the limit is {MAX_COMMAND_BYTES}",
                command.len()
            )));
        }
        let id = self.alloc_id();
        let sentinel = self.alloc_id();
        let deadline = Instant::now() + timeout;
        self.send(id, SERVERDATA_EXECCOMMAND, command).await?;
        self.send(sentinel, SERVERDATA_RESPONSE_VALUE, "").await?;
        let mut reply = String::new();
        let mut started = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = match (started, left.is_zero()) {
                (true, _) => REPLY_QUIET.min(left.max(Duration::from_millis(1))),
                (false, true) => return Err(RconError::Timeout(timeout)),
                (false, false) => left,
            };
            let next = tokio::time::timeout(wait, self.next_packet()).await;
            let p = match next {
                Err(_) if started => return Ok(reply),
                Err(_) => return Err(RconError::Timeout(timeout)),
                Ok(p) => p?,
            };
            if p.id == sentinel {
                return Ok(reply);
            }
            if p.id == REJECTED_ID {
                return Err(RconError::NotAuthenticated);
            }
            if p.id == id {
                started = true;
                reply.push_str(&p.body);
                if reply.len() > MAX_REPLY_BYTES {
                    return Err(RconError::Protocol(format!(
                        "reply exceeds {MAX_REPLY_BYTES} bytes"
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_documented_layout() {
        // size = id 4 + type 4 + body 3 + two NULs = 13
        let bytes = encode(7, SERVERDATA_EXECCOMMAND, "abc");
        assert_eq!(&bytes[..4], &13i32.to_le_bytes());
        assert_eq!(&bytes[4..8], &7i32.to_le_bytes());
        assert_eq!(&bytes[8..12], &2i32.to_le_bytes());
        assert_eq!(&bytes[12..], b"abc\0\0");
        let (p, used) = decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(
            p,
            Packet {
                id: 7,
                ty: 2,
                body: "abc".into()
            }
        );
    }

    #[test]
    fn decode_waits_for_a_whole_packet_and_rejects_bad_sizes() {
        let bytes = encode(1, 0, "hello");
        assert!(decode(&bytes[..bytes.len() - 1]).unwrap().is_none());
        assert!(decode(&[]).unwrap().is_none());
        let small = 3i32.to_le_bytes();
        assert!(matches!(decode(&small), Err(RconError::Protocol(_))));
        let huge = (MAX_PACKET_SIZE + 1).to_le_bytes();
        assert!(matches!(decode(&huge), Err(RconError::Protocol(_))));
    }

    #[test]
    fn decode_rejects_a_missing_terminator() {
        let mut bytes = encode(1, 0, "hi");
        let n = bytes.len();
        bytes[n - 1] = b'x';
        assert!(matches!(decode(&bytes), Err(RconError::Protocol(_))));
    }

    #[tokio::test]
    async fn a_command_longer_than_the_protocol_allows_is_refused_before_sending() {
        let (a, _b) = tokio::io::duplex(64);
        let mut c = RconClient::from_stream(a);
        let long = "x".repeat(MAX_COMMAND_BYTES + 1);
        assert!(matches!(
            c.exec(&long, Duration::from_secs(1)).await,
            Err(RconError::Protocol(_))
        ));
    }
}
