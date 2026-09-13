//! Wire protocol for the libp2p sync layer.
//!
//! Framing: 4-byte big-endian length prefix + serde_json payload, capped
//! at [`MAX_MSG_BYTES`]. All messages live in one tagged enum
//! ([`SyncMsg`]) so the request-response behaviour carries the whole
//! protocol (hello, catch-up, reconciliation) over one stream protocol.

use std::io;

use libp2p::futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::request_response;
use libp2p::swarm::StreamProtocol;
use serde::{Deserialize, Serialize};

use crate::action::Action;
use crate::doc::{DocId, VecClock};
use crate::store::DocState;

/// Protocol version. Bumped for incompatible changes; mismatched majors
/// are rejected in the hello handshake.
pub const PROTOCOL_VERSION: u32 = 2;
/// Stream protocol id.
pub const SYNC_PROTOCOL: &str = "/ph-reactor/sync/2.0.0";
/// Gossipsub topic for op fan-out.
pub const GOSSIPSUB_TOPIC: &str = "ph-reactor/docs/2.0.0";
/// Maximum framed message size.
pub const MAX_MSG_BYTES: u32 = 1 << 20;
/// Max actions per catch-up response (keeps frames bounded; `more` signals
/// the remainder).
pub const CATCH_UP_MAX_ACTIONS: usize = 64;

/// A doc summary: id, name, and per-doc clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSummary {
    pub id: DocId,
    pub name: String,
    pub clock: VecClock,
}

/// Handshake, sent by the dialer right after the transport connects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    /// The instance's human name (shown in settings pages).
    pub name: String,
    /// The dialer's peer id (base58) — informational; the transport
    /// already authenticates the key.
    pub peer_id: String,
    /// The dialer's ed25519 public key, hex (32 bytes).
    pub pubkey: String,
    /// Optional shared secret (value, not env name).
    pub token: Option<String>,
    /// Optional join-proof (set by the joiner when joining an invite):
    /// proves it holds a valid invite so the inviter can add a drive back.
    #[serde(default)]
    pub accept: Option<crate::p2p::invite::InviteAccept>,
}

/// Successful handshake reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloAck {
    pub version: u32,
    pub name: String,
    /// All docs the responder knows (live and deleted), with clocks —
    /// lets the dialer plan catch-up immediately.
    pub docs: Vec<DocSummary>,
    /// The responder's ed25519 public key, hex (32 bytes). The channel
    /// is authenticated by the transport, so this is trusted; it lets
    /// the dialer register the responder's signing key even when the
    /// responder never dials back.
    #[serde(default)]
    pub pubkey: Option<String>,
}

/// Typed handshake rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HelloError {
    BadVersion { expected: u32, got: u32 },
    AuthRequired,
    BadToken,
    KeyMismatch,
    /// The peer is on the local ban list.
    Banned,
}

impl HelloError {
    pub fn as_str(&self) -> &'static str {
        match self {
            HelloError::BadVersion { .. } => "bad-version",
            HelloError::AuthRequired => "auth-required",
            HelloError::BadToken => "bad-token",
            HelloError::KeyMismatch => "key-mismatch",
            HelloError::Banned => "banned",
        }
    }
}

/// Catch-up request: "send me what I'm missing for this doc".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatchUp {
    pub doc_id: DocId,
    /// The requester's per-doc clock.
    pub have: VecClock,
}

/// Catch-up reply: the responder's current state plus the ops the
/// requester's clock does not cover (capped at [`CATCH_UP_MAX_OPS`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatchUpAck {
    pub doc_id: DocId,
    /// `None` when the responder does not know the doc.
    pub state: Option<DocState>,
    pub actions: Vec<Action>,
    /// True when more actions exist beyond the cap (re-request with the
    /// updated clock).
    pub more: bool,
}

/// Reconciliation request: our per-doc clocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub clocks: Vec<(DocId, VecClock)>,
}

/// Reconciliation reply: the responder's per-doc clocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryAck {
    pub clocks: Vec<(DocId, VecClock)>,
}

/// Gossip payload: one signed action for mesh fan-out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionMsg {
    pub action: Action,
    /// The sender's instance name (diagnostics only).
    #[serde(default)]
    pub name: Option<String>,
}

/// All protocol messages in one tagged enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "p")]
pub enum SyncMsg {
    Hello(Hello),
    HelloAck(HelloAck),
    HelloErr(HelloError),
    CatchUp(CatchUp),
    CatchUpAck(CatchUpAck),
    Summary(Summary),
    SummaryAck(SummaryAck),
}

/// Length-prefixed JSON codec for the request-response behaviour.
#[derive(Debug, Clone)]
pub struct SyncCodec {
    max_len: u32,
}

impl SyncCodec {
    pub fn new() -> Self {
        Self {
            max_len: MAX_MSG_BYTES,
        }
    }
}

impl Default for SyncCodec {
    fn default() -> Self {
        Self::new()
    }
}

async fn read_msg<T>(io: &mut T, max_len: u32) -> io::Result<SyncMsg>
where
    T: AsyncRead + Unpin,
{
    let mut lenbuf = [0u8; 4];
    io.read_exact(&mut lenbuf).await?;
    let len = u32::from_be_bytes(lenbuf);
    if len > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message too large: {len} > {max_len}"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    io.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad json: {e}")))
}

async fn write_msg<T>(io: &mut T, msg: &SyncMsg) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("serialize: {e}")))?;
    if body.len() as u64 > MAX_MSG_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "encoded message too large",
        ));
    }
    io.write_all(&(body.len() as u32).to_be_bytes()).await?;
    io.write_all(&body).await?;
    io.flush().await
}

#[async_trait::async_trait]
impl request_response::Codec for SyncCodec {
    type Protocol = StreamProtocol;
    type Request = SyncMsg;
    type Response = SyncMsg;

    async fn read_request<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<SyncMsg>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io, self.max_len).await
    }

    async fn read_response<T>(&mut self, _: &Self::Protocol, io: &mut T) -> io::Result<SyncMsg>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io, self.max_len).await
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: SyncMsg,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &req).await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: SyncMsg,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &res).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn codec_round_trips_all_message_kinds() {
        let msg = SyncMsg::Hello(Hello {
            version: PROTOCOL_VERSION,
            name: "alpha".into(),
            peer_id: "12D34".into(),
            pubkey: "00".repeat(32),
            token: Some("s3cret".into()),
            accept: None,
        });
        let cases = vec![
            msg,
            SyncMsg::HelloAck(HelloAck {
                version: PROTOCOL_VERSION,
                name: "beta".into(),
                docs: vec![DocSummary {
                    id: DocId::new(),
                    name: "note-1".into(),
                    clock: VecClock::default(),
                }],
                pubkey: Some("ab".repeat(32)),
            }),
            SyncMsg::HelloErr(HelloError::BadToken),
            SyncMsg::CatchUp(CatchUp {
                doc_id: DocId::new(),
                have: VecClock::default(),
            }),
            SyncMsg::CatchUpAck(CatchUpAck {
                doc_id: DocId::new(),
                state: None,
                actions: Vec::new(),
                more: true,
            }),
            SyncMsg::Summary(Summary { clocks: Vec::new() }),
            SyncMsg::SummaryAck(SummaryAck { clocks: Vec::new() }),
        ];
        for msg in cases {
            let mut buf = Vec::new();
            write_msg(&mut buf, &msg).await.unwrap();
            // length prefix + json
            let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
            assert_eq!(len as usize + 4, buf.len());
            let mut reader = &buf[..];
            let back = read_msg(&mut reader, MAX_MSG_BYTES).await.unwrap();
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn hello_ack_without_pubkey_still_decodes() {
        // Pre-pubkey wire shape (older peers mid-upgrade).
        let json = r#"{"t":"HelloAck","p":{"version":1,"name":"old","docs":[]}}"#;
        let msg: SyncMsg = serde_json::from_str(json).expect("decode");
        let SyncMsg::HelloAck(ack) = msg else {
            panic!("expected HelloAck");
        };
        assert_eq!(ack.pubkey, None);
    }
    #[tokio::test]
    async fn oversized_length_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_MSG_BYTES + 1).to_be_bytes());
        buf.push(b'x');
        let mut reader = &buf[..];
        assert!(read_msg(&mut reader, MAX_MSG_BYTES).await.is_err());
    }
}
