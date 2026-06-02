//! Client orchestrator and per-session state machine (plan §4.1, §4.3).
//!
//! One long-lived signaling connection multiplexes many sessions. Each `Punch`
//! notification spawns a [`run_session`] task that walks the state machine:
//! gather candidates → exchange → punch → try QUIC P2P → fall back to relay →
//! bridge RDP.

use anyhow::{bail, Context, Result};
use spuria_common::{
    candidate::SessionOffer,
    crypto::DeviceKey,
    ids::DeviceId,
    protocol::{ClientMsg, ErrorCode, ServerMsg, SessionId},
    transport::Role,
};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn};

use crate::{
    candidates, certs,
    certs::SelfSignedCert,
    rdp,
    signaling_client::{self, RegisterInfo, SignalingHandle},
    tunnel::{quic, relay, Tunnel},
};

/// Status events emitted by the state machine for a UI (the Tauri GUI) to
/// render. Serializable so they can be forwarded straight to the frontend.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientEvent {
    Registered {
        device_id: String,
    },
    SessionStarted {
        session_id: String,
        peer_id: String,
    },
    TunnelUp {
        session_id: String,
        path: String,
    },
    /// Controller: local listener bound and waiting for an RDP client.
    RdpReady {
        session_id: String,
        listen_addr: String,
    },
    /// Host: bridging the tunnel to the local RDP service.
    HostBridging {
        session_id: String,
        rdp_addr: String,
    },
    SessionEnded {
        session_id: String,
        error: Option<String>,
    },
    Error {
        message: String,
    },
}

/// Fully-resolved runtime configuration for the client.
pub struct AppConfig {
    pub role: Role,
    pub server_url: String,
    pub reflect_addr: SocketAddr,
    pub device_id: DeviceId,
    pub secret: String,
    /// Controller only: which peer to connect to.
    pub peer_id: Option<DeviceId>,
    /// Host only: local RDP service address.
    pub rdp_addr: SocketAddr,
    /// Controller only: local listener for the RDP client.
    pub listen_addr: SocketAddr,
    pub data_dir: PathBuf,
    /// Skip the QUIC P2P attempt and use the relay directly.
    pub force_relay: bool,
    /// Enable RDP UDP multitransport over the QUIC datagram channel (P3).
    pub enable_udp: bool,
    /// Optional channel for UI status events.
    pub events: Option<UnboundedSender<ClientEvent>>,
}

/// Shared, immutable-ish context handed to every session task.
struct Ctx {
    role: Role,
    signaling: SignalingHandle,
    device_key: Arc<DeviceKey>,
    cert: Arc<SelfSignedCert>,
    reflect_addr: SocketAddr,
    rdp_addr: SocketAddr,
    listen_addr: SocketAddr,
    force_relay: bool,
    enable_udp: bool,
    events: Option<UnboundedSender<ClientEvent>>,
}

impl Ctx {
    fn emit(&self, ev: ClientEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(ev);
        }
    }
}

pub async fn run(cfg: AppConfig) -> Result<()> {
    // Install the ring crypto provider for rustls/QUIC (idempotent across calls).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let device_key = Arc::new(DeviceKey::load_or_generate(
        &cfg.data_dir.join("device_key.bin"),
    )?);
    let cert = Arc::new(certs::generate()?);
    info!(
        device_id = %cfg.device_id,
        role = ?cfg.role,
        noise_pubkey = %device_key.public_hex(),
        "starting spuria client"
    );

    let (signaling, mut inbound) = signaling_client::connect(
        &cfg.server_url,
        RegisterInfo {
            device_id: cfg.device_id.clone(),
            noise_pubkey: device_key.public_hex(),
            secret: cfg.secret.clone(),
            display_name: None,
        },
    )
    .await
    .context("connecting to signaling")?;

    let ctx = Arc::new(Ctx {
        role: cfg.role,
        signaling: signaling.clone(),
        device_key,
        cert,
        reflect_addr: cfg.reflect_addr,
        rdp_addr: cfg.rdp_addr,
        listen_addr: cfg.listen_addr,
        force_relay: cfg.force_relay,
        enable_udp: cfg.enable_udp,
        events: cfg.events.clone(),
    });

    let mut sessions: HashMap<SessionId, UnboundedSender<ServerMsg>> = HashMap::new();
    let mut connect_sent = false;

    while let Some(msg) = inbound.recv().await {
        match msg {
            ServerMsg::Registered {
                device_id,
                observed,
            } => {
                info!(%device_id, ?observed, "registered with signaling");
                ctx.emit(ClientEvent::Registered {
                    device_id: device_id.to_string(),
                });
                if ctx.role == Role::Controller && !connect_sent {
                    let peer = cfg
                        .peer_id
                        .clone()
                        .context("controller requires a --peer id")?;
                    info!(%peer, "requesting connection to peer");
                    signaling.send(ClientMsg::Connect { peer_id: peer });
                    connect_sent = true;
                }
            }
            ServerMsg::Punch {
                session_id,
                peer_id,
                peer_noise_pubkey,
                initiator,
            } => {
                info!(%session_id, %peer_id, initiator, "punch notify; starting session");
                let (tx, rx) = unbounded_channel();
                sessions.insert(session_id.clone(), tx);
                let ctx = ctx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    let res =
                        run_session(ctx.clone(), sid.clone(), peer_id, peer_noise_pubkey, rx).await;
                    let error = res.as_ref().err().map(|e| e.to_string());
                    if let Err(e) = &res {
                        warn!(session_id = %sid, error = %e, "session ended with error");
                    }
                    ctx.emit(ClientEvent::SessionEnded {
                        session_id: sid,
                        error,
                    });
                });
            }
            ServerMsg::Error { code, message } => {
                warn!(?code, %message, "signaling error");
                ctx.emit(ClientEvent::Error {
                    message: message.clone(),
                });
                // The host may not be online yet — retry the connection request.
                if ctx.role == Role::Controller && code == ErrorCode::PeerOffline {
                    if let Some(peer) = cfg.peer_id.clone() {
                        let signaling = signaling.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            info!(%peer, "retrying connection to peer");
                            signaling.send(ClientMsg::Connect { peer_id: peer });
                        });
                    }
                }
            }
            ServerMsg::Pong => {}
            other => {
                // Candidates / RelayAssign / PeerOffline — route to the session.
                if let Some(sid) = route_session_id(&other).cloned() {
                    if let Some(tx) = sessions.get(&sid) {
                        let _ = tx.send(other);
                    } else {
                        debug!(session_id = %sid, "message for unknown session");
                    }
                }
            }
        }
    }

    warn!("signaling connection closed");
    Ok(())
}

fn route_session_id(msg: &ServerMsg) -> Option<&SessionId> {
    match msg {
        ServerMsg::Candidates { session_id, .. }
        | ServerMsg::RelayAssign { session_id, .. }
        | ServerMsg::PeerOffline { session_id } => Some(session_id),
        _ => None,
    }
}

async fn run_session(
    ctx: Arc<Ctx>,
    session_id: SessionId,
    peer_id: DeviceId,
    peer_pubkey_hex: String,
    mut rx: UnboundedReceiver<ServerMsg>,
) -> Result<()> {
    let peer_pubkey = hex::decode(&peer_pubkey_hex).context("decoding peer noise pubkey")?;
    ctx.emit(ClientEvent::SessionStarted {
        session_id: session_id.clone(),
        peer_id: peer_id.to_string(),
    });

    // 1. Gather local candidates on a fresh UDP socket.
    let gathered = candidates::gather(ctx.reflect_addr)
        .await
        .context("gathering candidates")?;
    info!(%session_id, candidates = ?gathered.candidates, "gathered local candidates");

    // 2. Send our offer (candidates + QUIC fingerprint).
    ctx.signaling.send(ClientMsg::Candidates {
        session_id: session_id.clone(),
        offer: SessionOffer {
            candidates: gathered.candidates.clone(),
            quic_cert_fp: ctx.cert.fingerprint.clone(),
        },
    });

    // 3. Await the peer's offer.
    let peer_offer = wait_for(&mut rx, Duration::from_secs(15), |m| match m {
        ServerMsg::Candidates { offer, .. } => Some(offer),
        _ => None,
    })
    .await
    .context("awaiting peer candidates")?;
    let peer_addrs: Vec<SocketAddr> = peer_offer.candidates.iter().map(|c| c.addr).collect();
    info!(%session_id, %peer_id, ?peer_addrs, "received peer candidates");

    // 4. Punch open the NAT mappings.
    candidates::punch(&gathered.socket, &peer_addrs).await;

    // 5/6. Happy-eyeballs: pre-warm the relay concurrently with the QUIC P2P
    // attempt, prefer P2P, and fall back to the already-warmed relay if P2P
    // doesn't establish within the window. Both peers run this symmetrically;
    // the mutual QUIC handshake outcome keeps their path choice consistent.
    let tunnel = if ctx.force_relay {
        warn!(%session_id, "relay path forced");
        Tunnel::Relay(establish_relay(&ctx, &session_id, &peer_pubkey, &mut rx).await?)
    } else {
        // Request the relay ticket now so the fallback is ready immediately.
        ctx.signaling.send(ClientMsg::RequestRelay {
            session_id: session_id.clone(),
        });
        let relay_task = {
            let ctx = ctx.clone();
            let peer_pubkey = peer_pubkey.clone();
            tokio::spawn(async move {
                let (relay_addr, ticket) =
                    wait_for(&mut rx, Duration::from_secs(15), |m| match m {
                        ServerMsg::RelayAssign {
                            relay_addr, ticket, ..
                        } => Some((relay_addr, ticket)),
                        _ => None,
                    })
                    .await?;
                relay::connect(relay_addr, &ticket, ctx.role, &ctx.device_key, &peer_pubkey).await
            })
        };

        let socket = gathered
            .socket
            .into_std()
            .context("converting udp socket to std")?;
        match quic::attempt(
            socket,
            &ctx.cert,
            &peer_offer.quic_cert_fp,
            &peer_addrs,
            ctx.role,
            Duration::from_secs(3),
        )
        .await
        {
            Ok(t) => {
                info!(%session_id, "P2P QUIC tunnel established (relay pre-warm cancelled)");
                relay_task.abort();
                Tunnel::Quic(t)
            }
            Err(e) => {
                warn!(%session_id, error = %e, "P2P failed; using pre-warmed relay");
                let rt = relay_task
                    .await
                    .context("relay pre-warm task")?
                    .context("connecting relay tunnel")?;
                info!(%session_id, "relay tunnel established");
                Tunnel::Relay(rt)
            }
        }
    };

    ctx.emit(ClientEvent::TunnelUp {
        session_id: session_id.clone(),
        path: format!("{:?}", tunnel.path()).to_lowercase(),
    });

    // 7. Bridge RDP for our role.
    match ctx.role {
        Role::Host => ctx.emit(ClientEvent::HostBridging {
            session_id: session_id.clone(),
            rdp_addr: ctx.rdp_addr.to_string(),
        }),
        Role::Controller => ctx.emit(ClientEvent::RdpReady {
            session_id: session_id.clone(),
            listen_addr: ctx.listen_addr.to_string(),
        }),
    }
    let result = match ctx.role {
        Role::Host => rdp::serve_host(tunnel, ctx.rdp_addr, ctx.enable_udp).await,
        Role::Controller => rdp::serve_controller(tunnel, ctx.listen_addr, ctx.enable_udp).await,
    };

    ctx.signaling.send(ClientMsg::Bye { session_id });
    result
}

/// Request a relay ticket from signaling and connect the encrypted relay tunnel.
async fn establish_relay(
    ctx: &Ctx,
    session_id: &SessionId,
    peer_pubkey: &[u8],
    rx: &mut UnboundedReceiver<ServerMsg>,
) -> Result<relay::RelayTunnel> {
    ctx.signaling.send(ClientMsg::RequestRelay {
        session_id: session_id.clone(),
    });
    let (relay_addr, ticket) = wait_for(rx, Duration::from_secs(15), |m| match m {
        ServerMsg::RelayAssign {
            relay_addr, ticket, ..
        } => Some((relay_addr, ticket)),
        _ => None,
    })
    .await
    .context("awaiting relay assignment")?;
    let rt = relay::connect(relay_addr, &ticket, ctx.role, &ctx.device_key, peer_pubkey)
        .await
        .context("connecting relay tunnel")?;
    info!(%session_id, %relay_addr, "relay tunnel established");
    Ok(rt)
}

/// Await the first inbound message that `pick` maps to `Some`, aborting on
/// `PeerOffline`, timeout, or channel close.
async fn wait_for<T>(
    rx: &mut UnboundedReceiver<ServerMsg>,
    dur: Duration,
    pick: impl Fn(ServerMsg) -> Option<T>,
) -> Result<T> {
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        let msg = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("timed out"))?
            .ok_or_else(|| anyhow::anyhow!("session channel closed"))?;
        if matches!(msg, ServerMsg::PeerOffline { .. }) {
            bail!("peer went offline");
        }
        if let Some(v) = pick(msg) {
            return Ok(v);
        }
    }
}
