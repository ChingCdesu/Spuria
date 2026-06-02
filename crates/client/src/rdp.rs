//! RDP integration points (plan §3.4).
//!
//! * Host (被控): bridge the tunnel directly to the local system RDP service on
//!   `127.0.0.1:3389`.
//! * Controller (主控): expose a local TCP listener and bridge the first RDP
//!   client connection over the tunnel.
//!
//! UDP MULTITRANSPORT (P3, plan §3.4, §4.2): on the QUIC path each side also
//! runs an L4 UDP forwarder ([`quic::host_udp_forward`] /
//! [`quic::controller_udp_forward`]) alongside the TCP bridge, so the RDP
//! client's RDPEUDP/RDPEMT traffic traverses the tunnel's datagram channel. On
//! the relay path UDP multitransport degrades to TCP-only (plan §4.2).
//!
//! This generic local-listener bridge works with any RDP client (mstsc). The
//! plan's alternative — embedding IronRDP in the controller with no loopback
//! port — is an optional refinement (decision #6, gated on the IronRDP UDP
//! capability spike); the functional goal (UDP multitransport over P2P) is met
//! here without it.

use anyhow::{Context, Result};
use spuria_common::transport::Role;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tracing::info;

use crate::tunnel::{quic, Tunnel};

/// 被控: connect to the local RDP service and bridge it onto the tunnel.
pub async fn serve_host(tunnel: Tunnel, rdp_addr: SocketAddr, enable_udp: bool) -> Result<()> {
    let local = TcpStream::connect(rdp_addr)
        .await
        .with_context(|| format!("connecting to local RDP service at {rdp_addr}"))?;
    info!(%rdp_addr, path = ?tunnel.path(), enable_udp, "host: bridging tunnel to local RDP");
    let udp = if enable_udp {
        tunnel.datagram_conn()
    } else {
        None
    };
    let reliable = tunnel.bridge(local, Role::Host);
    match udp {
        Some(conn) => tokio::select! {
            r = reliable => r,
            r = quic::host_udp_forward(conn, rdp_addr) => r,
        },
        None => reliable.await,
    }
}

/// 主控: bind a local listener and bridge the first RDP client over the tunnel.
pub async fn serve_controller(
    tunnel: Tunnel,
    listen_addr: SocketAddr,
    enable_udp: bool,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("binding local RDP listener at {listen_addr}"))?;
    let local_addr = listener.local_addr()?;
    info!(
        path = ?tunnel.path(),
        "RDP tunnel READY — point your RDP client (mstsc) at {local_addr}"
    );
    let (local, peer) = listener
        .accept()
        .await
        .context("accepting local RDP client")?;
    info!(%peer, "local RDP client connected; bridging over tunnel");
    let udp = if enable_udp {
        tunnel.datagram_conn()
    } else {
        None
    };
    let reliable = tunnel.bridge(local, Role::Controller);
    match udp {
        Some(conn) => tokio::select! {
            r = reliable => r,
            r = quic::controller_udp_forward(conn, listen_addr) => r,
        },
        None => reliable.await,
    }
}
