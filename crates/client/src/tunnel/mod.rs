//! Tunnel abstraction (plan §4.2). Two concrete paths behind one enum; both
//! expose a primary reliable byte stream for the main RDP connection.
//!
//! The unreliable datagram channel (for legacy lossy UDP multitransport, plan
//! §3.4 / P3) is only meaningful on the QUIC path; see [`quic`] for the hooks.

pub mod quic;
pub mod relay;

use anyhow::Result;
use spuria_common::transport::{Role, TunnelPath};
use tokio::net::TcpStream;

pub enum Tunnel {
    Quic(quic::QuicTunnel),
    Relay(relay::RelayTunnel),
}

impl Tunnel {
    pub fn path(&self) -> TunnelPath {
        match self {
            Tunnel::Quic(_) => TunnelPath::P2p,
            Tunnel::Relay(_) => TunnelPath::Relay,
        }
    }

    /// A connection handle for the unreliable datagram channel, when the path
    /// supports it (QUIC only). Used to run UDP multitransport forwarding
    /// alongside the reliable TCP bridge.
    pub fn datagram_conn(&self) -> Option<quinn::Connection> {
        match self {
            Tunnel::Quic(t) => Some(t.connection()),
            Tunnel::Relay(_) => None,
        }
    }

    /// Bridge the tunnel's reliable channel with a local TCP stream until either
    /// side closes. `role` decides who opens vs. accepts the QUIC stream.
    pub async fn bridge(self, local: TcpStream, role: Role) -> Result<()> {
        match self {
            Tunnel::Quic(t) => t.bridge(local, role).await,
            Tunnel::Relay(t) => t.bridge(local, role).await,
        }
    }
}
