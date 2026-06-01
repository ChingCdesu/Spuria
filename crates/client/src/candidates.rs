//! Candidate gathering and UDP hole punching (plan §3.3, §4.1 steps 3-4).
//!
//! We bind one UDP socket, learn a host candidate (the local interface address)
//! and a server-reflexive candidate (via the reflection service), then hand the
//! *same* socket to QUIC so the punched NAT mapping is reused.

use anyhow::{Context, Result};
use spuria_common::{
    candidate::{Candidate, CandidateKind},
    protocol::reflect,
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::debug;

/// Small payload used to pry open NAT mappings before QUIC takes over.
const PUNCH_PAYLOAD: &[u8] = b"SPURIA-PUNCH";

pub struct Gathered {
    pub socket: UdpSocket,
    pub candidates: Vec<Candidate>,
}

/// Bind a UDP socket and collect host + srflx candidates.
pub async fn gather(reflect_addr: SocketAddr) -> Result<Gathered> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .context("binding UDP socket")?;
    let port = socket.local_addr()?.port();

    let mut candidates = Vec::new();
    if let Some(ip) = local_ip_toward(reflect_addr) {
        candidates.push(Candidate::new(
            CandidateKind::Host,
            SocketAddr::new(ip, port),
        ));
    }

    // Server-reflexive: ask the reflection service what address it observed.
    let _ = socket.send_to(&reflect::request(), reflect_addr).await;
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) => {
            if let Some(observed) = reflect::parse_response(&buf[..n]) {
                candidates.push(Candidate::new(CandidateKind::Srflx, observed));
            }
        }
        _ => debug!("no srflx reflection received"),
    }

    Ok(Gathered { socket, candidates })
}

/// Send a burst of punch packets toward every peer candidate to open the local
/// NAT mapping before the QUIC handshake.
pub async fn punch(socket: &UdpSocket, targets: &[SocketAddr]) {
    for _ in 0..6 {
        for t in targets {
            let _ = socket.send_to(PUNCH_PAYLOAD, t).await;
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

/// Determine which local interface address would be used to reach `target`,
/// without sending any packets.
fn local_ip_toward(target: SocketAddr) -> Option<IpAddr> {
    let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect(target).ok()?;
    probe.local_addr().ok().map(|a| a.ip())
}
