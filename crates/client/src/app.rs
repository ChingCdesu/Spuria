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
use std::{
    collections::HashMap, future::Future, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
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
    run_until_shutdown(cfg, std::future::pending()).await
}

struct SessionControl {
    messages: UnboundedSender<ServerMsg>,
    stop: Option<oneshot::Sender<()>>,
}

/// Run until the supplied shutdown future resolves, then release and await all
/// sessions and signaling tasks before returning. Dropping the future also
/// aborts its owned task groups as a last-resort cancellation guard.
pub async fn run_until_shutdown(cfg: AppConfig, shutdown: impl Future<Output = ()>) -> Result<()> {
    tokio::pin!(shutdown);
    if cfg.role == Role::Controller && cfg.peer_id.is_none() {
        bail!("controller requires a --peer id");
    }
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

    let connection = signaling_client::connect(
        &cfg.server_url,
        RegisterInfo {
            device_id: cfg.device_id.clone(),
            noise_pubkey: device_key.public_hex(),
            secret: cfg.secret.clone(),
            display_name: None,
        },
    );
    let mut connection = tokio::select! {
        biased;
        _ = &mut shutdown => return Ok(()),
        result = connection => result.context("connecting to signaling")?,
    };
    let signaling = connection.handle.clone();

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

    let mut sessions: HashMap<SessionId, SessionControl> = HashMap::new();
    let mut tasks = JoinSet::new();
    let mut connect_sent = false;
    let mut retry_at = None;

    loop {
        let msg = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok(sid)) = completed {
                    sessions.remove(&sid);
                }
                continue;
            }
            _ = async {
                match retry_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                retry_at = None;
                if let Some(peer) = cfg.peer_id.clone() {
                    info!(%peer, "retrying connection to peer");
                    signaling.send(ClientMsg::Connect { peer_id: peer });
                }
                continue;
            }
            msg = connection.inbound.recv() => match msg {
                Some(msg) => msg,
                None => {
                    warn!("signaling connection closed");
                    break;
                }
            },
        };
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
                    let peer = cfg.peer_id.clone().expect("validated controller peer");
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
                retry_at = None;
                // Duplicate notifications must not orphan an older task or
                // let its completion remove a newer session's routing entry.
                if sessions.contains_key(&session_id) {
                    continue;
                }
                let (tx, rx) = unbounded_channel();
                let (stop, stopped) = oneshot::channel();
                sessions.insert(
                    session_id.clone(),
                    SessionControl {
                        messages: tx,
                        stop: Some(stop),
                    },
                );
                let ctx = ctx.clone();
                let sid = session_id.clone();
                tasks.spawn(async move {
                    let res = until_stopped(
                        run_session(ctx.clone(), sid.clone(), peer_id, peer_noise_pubkey, rx),
                        stopped,
                    )
                    .await;
                    // End every session, including setup failures and peer or
                    // user cancellation, not only successful RDP bridges.
                    ctx.signaling.send(ClientMsg::Bye {
                        session_id: sid.clone(),
                    });
                    let error = res.as_ref().err().map(|e| e.to_string());
                    if let Err(e) = &res {
                        warn!(session_id = %sid, error = %e, "session ended with error");
                    }
                    ctx.emit(ClientEvent::SessionEnded {
                        session_id: sid.clone(),
                        error,
                    });
                    sid
                });
            }
            ServerMsg::Error { code, message } => {
                warn!(?code, %message, "signaling error");
                ctx.emit(ClientEvent::Error {
                    message: message.clone(),
                });
                // The host may not be online yet — retry the connection request.
                if ctx.role == Role::Controller && code == ErrorCode::PeerOffline {
                    retry_at = Some(tokio::time::Instant::now() + Duration::from_secs(3));
                }
            }
            ServerMsg::Pong => {}
            ServerMsg::PeerOffline { session_id } => {
                // The session may already be forwarding RDP and no longer
                // reading setup messages. Cancel its entire future directly.
                if let Some(stop) = sessions
                    .get_mut(&session_id)
                    .and_then(|session| session.stop.take())
                {
                    let _ = stop.send(());
                }
            }
            other => {
                // Candidates / RelayAssign / PeerOffline — route to the session.
                if let Some(sid) = route_session_id(&other).cloned() {
                    if let Some(session) = sessions.get(&sid) {
                        let _ = session.messages.send(other);
                    } else {
                        debug!(session_id = %sid, "message for unknown session");
                    }
                }
            }
        }
    }

    for (_, session) in sessions.drain() {
        if let Some(stop) = session.stop {
            let _ = stop.send(());
        }
    }
    // A completion means all scoped relay/UDP futures and their sockets have
    // been dropped. JoinSet additionally aborts them if our owner is dropped.
    while tasks.join_next().await.is_some() {}
    connection.shutdown().await;
    Ok(())
}

async fn until_stopped(
    work: impl Future<Output = Result<()>>,
    stopped: oneshot::Receiver<()>,
) -> Result<()> {
    tokio::select! {
        biased;
        _ = stopped => Ok(()),
        result = work => result,
    }
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
        let relay_attempt = {
            let ctx = ctx.clone();
            let peer_pubkey = peer_pubkey.clone();
            async move {
                let (relay_addr, ticket) =
                    wait_for(&mut rx, Duration::from_secs(15), |m| match m {
                        ServerMsg::RelayAssign {
                            relay_addr, ticket, ..
                        } => Some((relay_addr, ticket)),
                        _ => None,
                    })
                    .await?;
                relay::connect(relay_addr, &ticket, ctx.role, &ctx.device_key, &peer_pubkey).await
            }
        };
        tokio::pin!(relay_attempt);

        let socket = gathered
            .socket
            .into_std()
            .context("converting udp socket to std")?;
        let quic_attempt = quic::attempt(
            socket,
            &ctx.cert,
            &peer_offer.quic_cert_fp,
            &peer_addrs,
            ctx.role,
            Duration::from_secs(3),
        );
        tokio::pin!(quic_attempt);
        // Poll both attempts within this session. No detached pre-warm can
        // outlive cancellation or a successful P2P choice.
        let (quic_result, relay_result) = tokio::select! {
            result = &mut quic_attempt => (result, None),
            result = &mut relay_attempt => (quic_attempt.await, Some(result)),
        };
        match quic_result {
            Ok(t) => {
                info!(%session_id, "P2P QUIC tunnel established (relay pre-warm cancelled)");
                Tunnel::Quic(t)
            }
            Err(e) => {
                warn!(%session_id, error = %e, "P2P failed; using pre-warmed relay");
                let rt = match relay_result {
                    Some(result) => result,
                    None => relay_attempt.await,
                }
                .context("connecting relay tunnel")?;
                info!(%session_id, "relay tunnel established");
                Tunnel::Relay(rt)
            }
        }
    };

    ctx.signaling.send(ClientMsg::PathSelected {
        session_id: session_id.clone(),
        path: tunnel.path(),
    });
    ctx.emit(ClientEvent::TunnelUp {
        session_id: session_id.clone(),
        path: format!("{:?}", tunnel.path()).to_lowercase(),
    });

    // 7. Bridge RDP for our role.
    match ctx.role {
        Role::Host => {
            ctx.emit(ClientEvent::HostBridging {
                session_id: session_id.clone(),
                rdp_addr: ctx.rdp_addr.to_string(),
            });
            rdp::serve_host(tunnel, ctx.rdp_addr, ctx.enable_udp).await
        }
        Role::Controller => {
            rdp::serve_controller_ready(tunnel, ctx.listen_addr, ctx.enable_udp, |addr| {
                ctx.emit(ClientEvent::RdpReady {
                    session_id: session_id.clone(),
                    listen_addr: addr.to_string(),
                })
            })
            .await
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn cancellation_releases_listener_before_completion() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let (started, started_rx) = oneshot::channel();
        let mut sessions = JoinSet::new();
        sessions.spawn(until_stopped(
            async move {
                let _ = started.send(());
                listener.accept().await?;
                Ok(())
            },
            stopped,
        ));
        started_rx.await.unwrap();
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), sessions.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        let _replacement = TcpListener::bind(addr).await.unwrap();
    }

    #[tokio::test]
    async fn losing_owner_also_cancels_pending_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        drop(stop);
        until_stopped(
            async move {
                listener.accept().await?;
                Ok(())
            },
            stopped,
        )
        .await
        .unwrap();
        let _replacement = TcpListener::bind(addr).await.unwrap();
    }
}
