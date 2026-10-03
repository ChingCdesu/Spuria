//! The signaling control-plane wire protocol (plan §3.1, §4.1).
//!
//! Transport is WebSocket text frames carrying JSON. The plan nominally calls
//! for protobuf, but since the whole stack is Rust we use serde/JSON for v1:
//! debuggable, dependency-light, and trivially swappable behind these types.

use crate::{candidate::SessionOffer, ids::DeviceId, transport::TunnelPath};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Opaque per-connection session identifier minted by the signaling server.
pub type SessionId = String;

/// Opaque bearer token both peers present to the relay to be paired.
pub type Ticket = String;

/// Messages a client sends to the signaling server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First message on the socket: claim a device id and announce presence.
    Register {
        device_id: DeviceId,
        /// Hex-encoded Noise static public key (pinned by the peer later).
        noise_pubkey: String,
        /// Auth secret (see [`crate::auth`]).
        secret: String,
        #[serde(default)]
        display_name: Option<String>,
    },
    /// Keep-alive; resets the online-table TTL.
    Heartbeat,
    /// Controller asks to reach a controlled host (PeerQuery + PunchRequest).
    Connect { peer_id: DeviceId },
    /// Send my candidate set + QUIC fingerprint to the peer (CandidateExchange).
    Candidates {
        session_id: SessionId,
        offer: SessionOffer,
    },
    /// Ask the server to allocate a relay for this session.
    RequestRelay { session_id: SessionId },
    /// Report the path after the authenticated tunnel has been established.
    PathSelected {
        session_id: SessionId,
        path: TunnelPath,
    },
    /// Tear down a session.
    Bye { session_id: SessionId },
}

/// Messages the signaling server sends to a client.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Registration accepted. `observed` is the TCP source address the server
    /// saw — a weak hint only; the real srflx candidate comes from the UDP
    /// reflection service.
    Registered {
        device_id: DeviceId,
        observed: Option<SocketAddr>,
    },
    /// Both peers are notified to start punching (PunchNotify). The controller
    /// receives `initiator = true`.
    Punch {
        session_id: SessionId,
        peer_id: DeviceId,
        /// Hex-encoded Noise static public key of the peer (for pinning).
        peer_noise_pubkey: String,
        initiator: bool,
    },
    /// The peer's forwarded candidate set (CandidateExchange).
    Candidates {
        session_id: SessionId,
        offer: SessionOffer,
    },
    /// Relay ticket for this session; both peers receive the same ticket.
    RelayAssign {
        session_id: SessionId,
        relay_addr: SocketAddr,
        ticket: Ticket,
    },
    /// The peer went offline / the session was torn down.
    PeerOffline { session_id: SessionId },
    /// Heartbeat reply.
    Pong,
    /// Something went wrong.
    Error { code: ErrorCode, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthFailed,
    PeerOffline,
    BadRequest,
    Internal,
}

impl ClientMsg {
    pub fn to_text(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(self)?)
    }
    pub fn from_text(s: &str) -> crate::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

impl ServerMsg {
    pub fn to_text(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(self)?)
    }
    pub fn from_text(s: &str) -> crate::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

/// Minimal UDP reflection ("srflx") protocol — our STUN substitute (plan §3.1
/// "内置 STUN 能力"). The client sends [`REFLECT_MAGIC`] from the very socket it
/// will punch with; the server echoes back the observed source address so the
/// client learns its server-reflexive candidate.
pub mod reflect {
    use std::net::SocketAddr;

    /// 16-byte request marker.
    pub const REFLECT_MAGIC: &[u8; 16] = b"SPURIA-REFLECT01";

    /// Build the request datagram.
    pub fn request() -> Vec<u8> {
        REFLECT_MAGIC.to_vec()
    }

    /// Build the response datagram: magic followed by the observed address text.
    pub fn response(observed: SocketAddr) -> Vec<u8> {
        let mut v = REFLECT_MAGIC.to_vec();
        v.extend_from_slice(observed.to_string().as_bytes());
        v
    }

    /// Parse a response datagram into the observed address.
    pub fn parse_response(buf: &[u8]) -> Option<SocketAddr> {
        if buf.len() <= REFLECT_MAGIC.len() || &buf[..REFLECT_MAGIC.len()] != REFLECT_MAGIC {
            return None;
        }
        std::str::from_utf8(&buf[REFLECT_MAGIC.len()..])
            .ok()?
            .parse()
            .ok()
    }

    /// Is this datagram a reflection request?
    pub fn is_request(buf: &[u8]) -> bool {
        buf.len() == REFLECT_MAGIC.len() && buf == REFLECT_MAGIC
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Candidate, CandidateKind, SessionOffer};
    use std::net::SocketAddr;

    #[test]
    fn client_msg_roundtrip() {
        for m in [
            ClientMsg::Heartbeat,
            ClientMsg::Connect {
                peer_id: "123".into(),
            },
            ClientMsg::RequestRelay {
                session_id: "s".into(),
            },
            ClientMsg::Bye {
                session_id: "s".into(),
            },
            ClientMsg::PathSelected {
                session_id: "s".into(),
                path: TunnelPath::P2p,
            },
        ] {
            let back = ClientMsg::from_text(&m.to_text().unwrap()).unwrap();
            assert_eq!(format!("{m:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn server_msg_candidates_roundtrip() {
        let offer = SessionOffer {
            candidates: vec![Candidate::new(
                CandidateKind::Host,
                "127.0.0.1:5".parse().unwrap(),
            )],
            quic_cert_fp: "ab".into(),
            tcp_forwarding_v1: true,
        };
        let m = ServerMsg::Candidates {
            session_id: "s".into(),
            offer,
        };
        assert!(matches!(
            ServerMsg::from_text(&m.to_text().unwrap()).unwrap(),
            ServerMsg::Candidates { .. }
        ));
    }

    #[test]
    fn reflect_roundtrip() {
        let req = reflect::request();
        assert!(reflect::is_request(&req));
        let addr: SocketAddr = "203.0.113.5:1234".parse().unwrap();
        let resp = reflect::response(addr);
        assert!(!reflect::is_request(&resp));
        assert_eq!(reflect::parse_response(&resp), Some(addr));
        assert_eq!(reflect::parse_response(b"garbage"), None);
    }

    #[test]
    fn relay_hello_format() {
        let h = relay::hello("ticket123");
        assert_eq!(&h[..16], relay::RELAY_MAGIC);
        assert_eq!(
            u16::from_be_bytes([h[16], h[17]]) as usize,
            "ticket123".len()
        );
        assert_eq!(&h[18..], b"ticket123");
    }
}

/// Relay rendezvous framing (plan §3.2). A peer opens a TCP connection to the
/// relay and sends a single hello: `magic(16) || u16 BE ticket_len || ticket`.
/// The relay parks the first arrival and splices it to the second arrival that
/// presents the same ticket, then blindly forwards ciphertext both ways.
pub mod relay {
    /// 16-byte hello marker.
    pub const RELAY_MAGIC: &[u8; 16] = b"SPURIA-RELAY-001";

    /// Build the rendezvous hello frame for a ticket.
    pub fn hello(ticket: &str) -> Vec<u8> {
        let t = ticket.as_bytes();
        let mut v = Vec::with_capacity(RELAY_MAGIC.len() + 2 + t.len());
        v.extend_from_slice(RELAY_MAGIC);
        v.extend_from_slice(&(t.len() as u16).to_be_bytes());
        v.extend_from_slice(t);
        v
    }
}
