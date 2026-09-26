//! Signaling control-plane server: WebSocket accept loop + per-connection
//! message pump (plan §3.1, §4.1).

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use spuria_common::{
    auth::{Authenticator, Credentials},
    ids::DeviceId,
    protocol::{ClientMsg, ErrorCode, ServerMsg},
    ratelimit::RateLimiter,
};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{self, UnboundedSender},
        oneshot,
    },
};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::{
    admin, reflect,
    registry::{ConnectionId, Registry},
};

pub struct SignalingConfig {
    pub ws_bind: SocketAddr,
    pub reflect_bind: SocketAddr,
    /// Public address clients should dial to reach the relay.
    pub relay_addr: SocketAddr,
    /// Dedicated ticket signing key shared only with the relay server (32+ bytes).
    pub relay_secret: String,
    pub auth: Arc<dyn Authenticator>,
    /// If set (with `admin_token`), start the admin HTTP API on this address.
    pub admin_bind: Option<SocketAddr>,
    pub admin_token: Option<String>,
    /// Max sustained new connections per second per source IP (burst = 3x).
    pub max_conn_per_sec: f64,
}

pub async fn run(cfg: SignalingConfig) -> Result<()> {
    let registry = Arc::new(Registry::new(cfg.relay_addr, &cfg.relay_secret)?);

    // Reflection (srflx) service runs alongside the WS control plane.
    {
        let bind = cfg.reflect_bind;
        tokio::spawn(async move {
            if let Err(e) = reflect::run(bind).await {
                warn!(error = %e, "reflect service exited");
            }
        });
    }

    let listener = TcpListener::bind(cfg.ws_bind)
        .await
        .with_context(|| format!("binding signaling WS on {}", cfg.ws_bind))?;
    info!(addr = %cfg.ws_bind, relay = %cfg.relay_addr, "signaling listening");

    // Periodic online-table status (observability).
    {
        let registry = registry.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tick.tick().await;
                tracing::debug!(online = registry.online_count(), "online devices");
            }
        });
    }

    // Admin HTTP API (monitoring + management + audit), if configured.
    match (cfg.admin_bind, cfg.admin_token.clone()) {
        (Some(bind), Some(token)) => {
            let registry = registry.clone();
            tokio::spawn(async move {
                if let Err(e) = admin::serve(registry, bind, token).await {
                    warn!(error = %e, "admin server exited");
                }
            });
        }
        (Some(_), None) => {
            warn!("--admin-bind set without --admin-token; admin API NOT started");
        }
        _ => {}
    }

    let limiter = Arc::new(RateLimiter::new(
        (cfg.max_conn_per_sec * 3.0).max(10.0),
        cfg.max_conn_per_sec,
    ));
    let auth = cfg.auth;
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        if !limiter.allow(peer.ip()) {
            debug!(%peer, "connection rate limited");
            continue;
        }
        let registry = registry.clone();
        let auth = auth.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(registry, auth, tcp, peer).await {
                debug!(%peer, error = %e, "signaling connection ended");
            }
        });
    }
}

async fn handle_conn(
    registry: Arc<Registry>,
    auth: Arc<dyn Authenticator>,
    tcp: TcpStream,
    peer: SocketAddr,
) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(tcp)
        .await
        .context("websocket handshake")?;
    let (mut write, mut read) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    let (kick_tx, mut kick_rx) = oneshot::channel::<()>();

    // Outbound pump: everything we send to this client goes through `tx`.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg.to_text() {
                Ok(text) => {
                    if write.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!(error = %e, "failed to serialize server msg"),
            }
        }
        let _ = write.close().await;
    });

    // The first frame must be a valid, authenticated Register.
    let (device_id, connection_id) = match read.next().await {
        Some(Ok(Message::Text(t))) => {
            match register(&registry, &*auth, t.as_str(), peer, &tx, kick_tx) {
                Ok(id) => id,
                Err(e) => {
                    let _ = tx.send(ServerMsg::Error {
                        code: ErrorCode::AuthFailed,
                        message: e.to_string(),
                    });
                    drop(tx);
                    let _ = writer.await;
                    return Ok(());
                }
            }
        }
        _ => {
            drop(tx);
            let _ = writer.await;
            bail!("expected register frame");
        }
    };

    // Main control loop: process client frames, or stop on an admin kick.
    loop {
        tokio::select! {
            frame = read.next() => {
                let Some(frame) = frame else { break };
                let text = match frame {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue, // ignore binary / ping / pong
                    Err(e) => {
                        debug!(%peer, error = %e, "ws read error");
                        break;
                    }
                };
                match ClientMsg::from_text(text.as_str()) {
                    Ok(msg) => registry.with_connection(&device_id, connection_id, || {
                        dispatch(&registry, &device_id, &tx, msg);
                    }),
                    Err(e) => debug!(%peer, error = %e, "ignoring malformed client message"),
                }
            }
            _ = &mut kick_rx => {
                info!(%device_id, "connection closed by admin kick");
                break;
            }
        }
    }

    registry.unregister(&device_id, connection_id);
    drop(tx);
    let _ = writer.await;
    Ok(())
}

/// Validate and apply the Register frame; returns the registered device id.
fn register(
    registry: &Registry,
    auth: &dyn Authenticator,
    text: &str,
    peer: SocketAddr,
    tx: &UnboundedSender<ServerMsg>,
    kick: oneshot::Sender<()>,
) -> Result<(DeviceId, ConnectionId)> {
    let msg = ClientMsg::from_text(text).context("parsing register frame")?;
    let ClientMsg::Register {
        device_id,
        noise_pubkey,
        secret,
        ..
    } = msg
    else {
        bail!("first message must be register");
    };
    auth.authenticate(&Credentials {
        device_id: device_id.clone(),
        secret,
    })
    .map_err(|e| anyhow!("{e}"))?;

    let connection_id = registry.register(device_id.clone(), noise_pubkey, tx.clone(), kick);
    let _ = tx.send(ServerMsg::Registered {
        device_id: device_id.clone(),
        observed: Some(peer),
    });
    Ok((device_id, connection_id))
}

/// Route a control message from an already-registered device.
fn dispatch(registry: &Registry, me: &DeviceId, tx: &UnboundedSender<ServerMsg>, msg: ClientMsg) {
    match msg {
        ClientMsg::Register { .. } => {
            debug!(%me, "ignoring duplicate register");
        }
        ClientMsg::Heartbeat => {
            registry.touch(me);
            let _ = tx.send(ServerMsg::Pong);
        }
        ClientMsg::Connect { peer_id } => registry.start_session(me, &peer_id),
        ClientMsg::Candidates { session_id, offer } => {
            registry.forward_candidates(me, &session_id, offer)
        }
        ClientMsg::RequestRelay { session_id } => registry.assign_relay(me, &session_id),
        ClientMsg::PathSelected { session_id, path } => registry.record_path(me, &session_id, path),
        ClientMsg::Bye { session_id } => registry.close_session(me, &session_id),
    }
}
