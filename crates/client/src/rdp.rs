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
use tokio::net::{TcpListener, TcpStream, UdpSocket};
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
    serve_controller_ready(tunnel, listen_addr, enable_udp, |_| {}).await
}

/// Report readiness only after every required listener has bound. In
/// particular, port 0 is resolved once and reused for both TCP and UDP.
pub async fn serve_controller_ready(
    tunnel: Tunnel,
    listen_addr: SocketAddr,
    enable_udp: bool,
    ready: impl FnOnce(SocketAddr),
) -> Result<()> {
    let udp = if enable_udp {
        tunnel.datagram_conn()
    } else {
        None
    };
    let (listener, udp_socket) = bind_controller(listen_addr, udp.is_some(), ready).await?;
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
    let reliable = tunnel.bridge(local, Role::Controller);
    match (udp, udp_socket) {
        (Some(conn), Some(socket)) => tokio::select! {
            r = reliable => r,
            r = quic::controller_udp_forward_socket(conn, socket) => r,
        },
        _ => reliable.await,
    }
}

async fn bind_controller(
    listen_addr: SocketAddr,
    bind_udp: bool,
    ready: impl FnOnce(SocketAddr),
) -> Result<(TcpListener, Option<UdpSocket>)> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("binding local RDP listener at {listen_addr}"))?;
    let local_addr = listener.local_addr()?;
    let udp = if bind_udp {
        Some(
            UdpSocket::bind(local_addr)
                .await
                .with_context(|| format!("binding local UDP listener at {local_addr}"))?,
        )
    } else {
        None
    };
    ready(local_addr);
    Ok((listener, udp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[tokio::test]
    async fn ready_reports_bound_tcp_and_udp_address() {
        let reported = Cell::new(None);
        let (listener, udp) = bind_controller("127.0.0.1:0".parse().unwrap(), true, |addr| {
            assert_ne!(addr.port(), 0);
            assert!(std::net::TcpListener::bind(addr).is_err());
            assert!(std::net::UdpSocket::bind(addr).is_err());
            reported.set(Some(addr));
        })
        .await
        .unwrap();
        assert_eq!(reported.get(), Some(listener.local_addr().unwrap()));
        assert_eq!(
            udp.unwrap().local_addr().unwrap(),
            listener.local_addr().unwrap()
        );
    }

    #[tokio::test]
    async fn failed_tcp_or_udp_bind_never_reports_ready() {
        let ready = Cell::new(false);
        let occupied_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(
            bind_controller(occupied_tcp.local_addr().unwrap(), false, |_| ready
                .set(true))
            .await
            .is_err()
        );
        assert!(!ready.get());
        let occupied_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = occupied_udp.local_addr().unwrap();
        assert!(bind_controller(addr, true, |_| ready.set(true))
            .await
            .is_err());
        assert!(!ready.get());
        // A partially completed bind must also release its TCP listener.
        let _rebound = TcpListener::bind(addr).await.unwrap();
    }
}
