//! Shared transport vocabulary (plan §4.2 "传输抽象(双通道)").
//!
//! The concrete tunnels live in the `client` crate (they need tokio / quinn).
//! Here we only define the role and path enums everything agrees on, plus the
//! documented contract the tunnels implement.
//!
//! Contract — every tunnel exposes two channels:
//! * a **reliable byte stream** carrying the main RDP connection (and, on the
//!   P2P path, the reliable side of RDPEUDP2), and
//! * an **unreliable datagram** channel for legacy lossy multitransport.
//!
//! On the relay path only the reliable channel exists; datagrams degrade to
//! plain TCP RDP (plan §4.2).

use serde::{Deserialize, Serialize};

/// Which side of a session a client is playing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 主控 — embeds the RDP client, initiates the connection.
    Controller,
    /// 被控 — forwards the tunnel to the local system RDP service.
    Host,
}

impl Role {
    /// The controller always drives the Noise handshake and opens the primary
    /// reliable stream, regardless of which side won the QUIC race.
    pub fn is_initiator(self) -> bool {
        matches!(self, Role::Controller)
    }
}

/// Which physical path the tunnel ended up using.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelPath {
    /// Direct QUIC between peers (preferred). Reliable streams + datagrams.
    P2p,
    /// TCP + Noise via the relay server (fallback). Reliable only.
    Relay,
}

impl TunnelPath {
    pub fn supports_datagrams(self) -> bool {
        matches!(self, TunnelPath::P2p)
    }
}
