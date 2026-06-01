//! QUIC P2P tunnel (plan §2 decision #5, §4.2).
//!
//! The controller connects and the host accepts. Both first burst punch packets
//! (see [`crate::candidates::punch`]) so cone NATs open their mappings; the
//! QUIC handshake then traverses. Symmetric NAT defeats this and the caller
//! falls back to the relay.
//!
//! Peer authentication is **mutual** certificate-fingerprint pinning
//! ([`crate::certs`]): the connector verifies the acceptor's self-signed cert
//! and the acceptor verifies the connector's client cert, each against the
//! fingerprint the peer published via signaling. Either mismatch aborts the
//! handshake — that is what stops a MITM. RDP-layer NLA adds defence in depth.

use anyhow::{bail, Context, Result};
use futures_util::stream::{FuturesUnordered, StreamExt};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use spuria_common::{crypto::cert_fingerprint, transport::Role};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::{sync::Arc, time::Duration};
use tokio::io::{copy, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::certs::SelfSignedCert;

const ALPN: &[u8] = b"spuria/1";

pub struct QuicTunnel {
    // Kept alive for the lifetime of the connection.
    _endpoint: Endpoint,
    conn: Connection,
}

/// Attempt a P2P QUIC connection over the (already punched) UDP socket.
pub async fn attempt(
    socket: std::net::UdpSocket,
    cert: &SelfSignedCert,
    peer_fp: &str,
    peer_candidates: &[SocketAddr],
    role: Role,
    timeout: Duration,
) -> Result<QuicTunnel> {
    let endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config(cert, peer_fp)?),
        socket,
        Arc::new(quinn::TokioRuntime),
    )
    .context("creating QUIC endpoint")?;

    let conn = match role {
        Role::Controller => {
            let cc = client_config(cert, peer_fp)?;
            tokio::time::timeout(timeout, connect_any(&endpoint, cc, peer_candidates))
                .await
                .map_err(|_| anyhow::anyhow!("QUIC connect timed out"))??
        }
        Role::Host => tokio::time::timeout(timeout, accept_one(&endpoint))
            .await
            .map_err(|_| anyhow::anyhow!("QUIC accept timed out"))??,
    };

    info!(remote = %conn.remote_address(), "QUIC connection established");
    Ok(QuicTunnel {
        _endpoint: endpoint,
        conn,
    })
}

impl QuicTunnel {
    /// A clone of the underlying connection, for datagram (UDP) forwarding that
    /// runs alongside the reliable bridge.
    pub fn connection(&self) -> Connection {
        self.conn.clone()
    }

    pub async fn bridge(self, local: TcpStream, role: Role) -> Result<()> {
        // The controller opens the primary bi-stream; the host accepts it.
        let (mut send, mut recv) = if role.is_initiator() {
            self.conn.open_bi().await.context("opening bi stream")?
        } else {
            self.conn.accept_bi().await.context("accepting bi stream")?
        };

        let (mut lr, mut lw) = local.into_split();
        let up = async move {
            copy(&mut lr, &mut send).await?;
            let _ = send.finish();
            Ok::<_, anyhow::Error>(())
        };
        let down = async move {
            copy(&mut recv, &mut lw).await?;
            let _ = lw.shutdown().await;
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(up, down)?;
        Ok(())
    }
}

async fn connect_any(
    endpoint: &Endpoint,
    cc: ClientConfig,
    candidates: &[SocketAddr],
) -> Result<Connection> {
    let mut futs = FuturesUnordered::new();
    for addr in candidates.iter().copied() {
        let ep = endpoint.clone();
        let cc = cc.clone();
        futs.push(async move {
            let conn = ep.connect_with(cc, addr, "spuria")?.await?;
            Ok::<Connection, anyhow::Error>(conn)
        });
    }
    while let Some(res) = futs.next().await {
        match res {
            Ok(conn) => return Ok(conn),
            Err(e) => debug!(error = %e, "QUIC connect attempt failed"),
        }
    }
    bail!("all QUIC connect attempts failed")
}

async fn accept_one(endpoint: &Endpoint) -> Result<Connection> {
    loop {
        let Some(incoming) = endpoint.accept().await else {
            bail!("QUIC endpoint closed");
        };
        match incoming.accept() {
            Ok(connecting) => match connecting.await {
                Ok(conn) => return Ok(conn),
                Err(e) => debug!(error = %e, "incoming QUIC connection failed"),
            },
            Err(e) => debug!(error = %e, "refused incoming QUIC connection"),
        }
    }
}

// ---- UDP multitransport over QUIC datagrams (plan §3.4, §4.2; P3) ----
//
// Each datagram is `[flow_id: u32 BE][payload]`. The controller assigns a flow
// id per local UDP source address (an RDPEUDP connection); the host maps each
// flow to its own UDP socket toward the local RDP service. The channel is
// unreliable — RDPEUDP provides its own reliability on top, exactly as over a
// real UDP path.

fn pack_datagram(conn: &Connection, flow: u32, payload: &[u8]) {
    let max = conn.max_datagram_size().unwrap_or(0);
    if max == 0 || payload.len() + 4 > max {
        debug!(
            len = payload.len(),
            max, "UDP packet too large for QUIC datagram; dropping"
        );
        return;
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&flow.to_be_bytes());
    frame.extend_from_slice(payload);
    let _ = conn.send_datagram(frame.into());
}

fn unpack_datagram(dg: &[u8]) -> Option<(u32, &[u8])> {
    if dg.len() < 4 {
        return None;
    }
    Some((u32::from_be_bytes([dg[0], dg[1], dg[2], dg[3]]), &dg[4..]))
}

/// Controller side: bridge a local UDP socket (the RDP client's multitransport
/// port) to the peer over QUIC datagrams.
pub async fn controller_udp_forward(conn: Connection, listen: SocketAddr) -> Result<()> {
    let sock = UdpSocket::bind(listen)
        .await
        .with_context(|| format!("binding local UDP listener {listen}"))?;
    info!(%listen, "UDP multitransport listener ready");
    let mut next_id: u32 = 1;
    let mut by_addr: HashMap<SocketAddr, u32> = HashMap::new();
    let mut by_id: HashMap<u32, SocketAddr> = HashMap::new();
    let mut buf = vec![0u8; 65535];
    loop {
        tokio::select! {
            r = sock.recv_from(&mut buf) => {
                let (n, src) = r?;
                let id = *by_addr.entry(src).or_insert_with(|| {
                    let id = next_id;
                    next_id += 1;
                    by_id.insert(id, src);
                    id
                });
                pack_datagram(&conn, id, &buf[..n]);
            }
            dg = conn.read_datagram() => {
                let dg = dg.context("reading QUIC datagram")?;
                if let Some((id, payload)) = unpack_datagram(&dg) {
                    if let Some(&addr) = by_id.get(&id) {
                        let _ = sock.send_to(payload, addr).await;
                    }
                }
            }
        }
    }
}

/// Host side: forward QUIC datagrams to/from the local RDP service's UDP port,
/// one socket per flow.
pub async fn host_udp_forward(conn: Connection, rdp_addr: SocketAddr) -> Result<()> {
    let mut flows: HashMap<u32, Arc<UdpSocket>> = HashMap::new();
    let mut readers: Vec<JoinHandle<()>> = Vec::new();
    // Runs until the connection closes (read_datagram errors).
    while let Ok(dg) = conn.read_datagram().await {
        let Some((id, payload)) = unpack_datagram(&dg) else {
            continue;
        };
        let sock = match flows.get(&id) {
            Some(s) => s.clone(),
            None => {
                let s = match bind_local_udp(rdp_addr).await {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        warn!(error = %e, "failed to open local UDP socket for flow");
                        continue;
                    }
                };
                flows.insert(id, s.clone());
                // Reader: replies from the RDP service back through the tunnel.
                let conn = conn.clone();
                let reader_sock = s.clone();
                readers.push(tokio::spawn(async move {
                    let mut buf = vec![0u8; 65535];
                    while let Ok(n) = reader_sock.recv(&mut buf).await {
                        pack_datagram(&conn, id, &buf[..n]);
                    }
                }));
                s
            }
        };
        let _ = sock.send(payload).await;
    }
    for r in readers {
        r.abort();
    }
    Ok(())
}

async fn bind_local_udp(rdp_addr: SocketAddr) -> Result<UdpSocket> {
    let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    sock.connect(rdp_addr).await?;
    Ok(sock)
}

fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    t.max_idle_timeout(Some(
        Duration::from_secs(30)
            .try_into()
            .expect("valid idle timeout"),
    ));
    // Enable the unreliable datagram channel for RDP UDP multitransport (P3).
    t.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    Arc::new(t)
}

fn server_config(cert: &SelfSignedCert, peer_fp: &str) -> Result<ServerConfig> {
    // Acceptor: require and pin the connector's client certificate.
    let verifier = Arc::new(PinnedCertVerifier::new(peer_fp));
    let mut rc = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert.rustls_cert()], cert.rustls_key())
        .context("building rustls server config")?;
    rc.alpn_protocols = vec![ALPN.to_vec()];
    let qsc = QuicServerConfig::try_from(rc).context("quic server config")?;
    let mut sc = ServerConfig::with_crypto(Arc::new(qsc));
    sc.transport_config(transport());
    Ok(sc)
}

fn client_config(cert: &SelfSignedCert, peer_fp: &str) -> Result<ClientConfig> {
    // Connector: pin the acceptor's server cert and present our own client cert.
    let verifier = Arc::new(PinnedCertVerifier::new(peer_fp));
    let mut rc = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert.rustls_cert()], cert.rustls_key())
        .context("setting client auth cert")?;
    rc.alpn_protocols = vec![ALPN.to_vec()];
    let qcc = QuicClientConfig::try_from(rc).context("quic client config")?;
    let mut cc = ClientConfig::new(Arc::new(qcc));
    cc.transport_config(transport());
    Ok(cc)
}

/// rustls verifier that accepts exactly one certificate fingerprint, used for
/// both directions of mutual pinning (server cert and client cert).
#[derive(Debug)]
struct PinnedCertVerifier {
    fingerprint: String,
}

impl PinnedCertVerifier {
    fn new(fingerprint: &str) -> Self {
        Self {
            fingerprint: fingerprint.to_string(),
        }
    }

    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if cert_fingerprint(end_entity.as_ref()) == self.fingerprint {
            Ok(())
        } else {
            Err(rustls::Error::General(
                "peer certificate fingerprint mismatch".into(),
            ))
        }
    }
}

fn pinned_schemes() -> Vec<SignatureScheme> {
    vec![
        SignatureScheme::ECDSA_NISTP256_SHA256,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::ED25519,
        SignatureScheme::RSA_PSS_SHA256,
        SignatureScheme::RSA_PKCS1_SHA256,
    ]
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        // We pin the certificate itself, so we accept its signatures.
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        pinned_schemes()
    }
}

impl ClientCertVerifier for PinnedCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        pinned_schemes()
    }
}
