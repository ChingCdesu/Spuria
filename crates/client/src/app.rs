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
    collections::{HashMap, HashSet},
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc::{self, unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::{
    candidates, certs,
    certs::SelfSignedCert,
    forwarding::{self, ForwardCommand, ForwardEvent, ForwardHandle, ForwardRule},
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
    PortForwardingAvailable {
        session_id: String,
        available: bool,
    },
    PortForwardStarted {
        session_id: String,
        forward_id: String,
        listen_addr: String,
        remote_port: u16,
    },
    PortForwardStopped {
        session_id: String,
        forward_id: String,
    },
    PortForwardError {
        session_id: String,
        forward_id: String,
        message: String,
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
    /// Host only: explicitly permitted remote loopback TCP ports.
    pub allow_forward_ports: Vec<u16>,
    /// Controller only: rules to start once the forwarding mux is ready.
    pub initial_forwards: Vec<ForwardRule>,
    /// Optional bounded UI/automation control receiver, owned by this run.
    pub controls: Option<AppControlReceiver>,
}

const CONTROL_CAPACITY: usize = 32;
const FORWARD_PENDING: u8 = 0;
const FORWARD_LEGACY: u8 = 1;
const FORWARD_READY: u8 = 2;

enum ControlCommand {
    Start {
        session_id: SessionId,
        rule: ForwardRule,
        reply: oneshot::Sender<std::result::Result<SocketAddr, String>>,
    },
    Stop {
        session_id: SessionId,
        id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
}

/// Commands are scoped to a specific running app and session. A handle from a
/// disconnected run cannot address a later run, even if a device ID is reused.
#[derive(Clone)]
pub struct AppControl {
    sender: mpsc::Sender<ControlCommand>,
}

pub struct AppControlReceiver(mpsc::Receiver<ControlCommand>);

pub fn control_channel() -> (AppControl, AppControlReceiver) {
    let (sender, receiver) = mpsc::channel(CONTROL_CAPACITY);
    (AppControl { sender }, AppControlReceiver(receiver))
}

impl AppControl {
    pub async fn start(
        &self,
        session_id: SessionId,
        rule: ForwardRule,
    ) -> std::result::Result<SocketAddr, String> {
        let (reply, response) = oneshot::channel();
        self.sender
            .try_send(ControlCommand::Start {
                session_id,
                rule,
                reply,
            })
            .map_err(|error| format!("forwarding command unavailable: {error}"))?;
        response
            .await
            .map_err(|_| "forwarding session ended before the command completed".to_string())?
    }

    pub async fn stop(&self, session_id: SessionId, id: String) -> std::result::Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.sender
            .try_send(ControlCommand::Stop {
                session_id,
                id,
                reply,
            })
            .map_err(|error| format!("forwarding command unavailable: {error}"))?;
        response
            .await
            .map_err(|_| "forwarding session ended before the command completed".to_string())?
    }
}

/// Validate before connecting to signaling or writing local device state.
pub fn validate_forward_ports(ports: &[u16]) -> std::result::Result<(), String> {
    if ports.len() > 128 {
        return Err("at most 128 allowed forwarding ports may be configured".into());
    }
    let mut seen = HashSet::new();
    for &port in ports {
        if port == 0 {
            return Err("allowed forwarding ports must be nonzero".into());
        }
        if !seen.insert(port) {
            return Err(format!("duplicate allowed forwarding port {port}"));
        }
    }
    Ok(())
}

pub fn validate_forward_rule(rule: &ForwardRule) -> std::result::Result<(), String> {
    forwarding::validate_rule(rule)
}

fn validate_initial_forwards(rules: &[ForwardRule]) -> std::result::Result<(), String> {
    if rules.len() > 32 {
        return Err("at most 32 forwarding rules may be configured".into());
    }
    let mut ids = HashSet::new();
    for rule in rules {
        validate_forward_rule(rule)?;
        if !ids.insert(&rule.id) {
            return Err(format!("duplicate forwarding rule ID {}", rule.id));
        }
    }
    Ok(())
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
    allow_forward_ports: Vec<u16>,
    initial_forwards: Vec<ForwardRule>,
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
    forwarding: ForwardHandle,
    forwarding_status: Arc<AtomicU8>,
}

struct SessionForwarding {
    handle: ForwardHandle,
    commands: mpsc::Receiver<ForwardCommand>,
    status: Arc<AtomicU8>,
}

fn route_control(
    role: Role,
    sessions: &HashMap<SessionId, SessionControl>,
    command: ControlCommand,
    emit: impl Fn(ClientEvent),
) {
    let (session_id, command) = match command {
        ControlCommand::Start {
            session_id,
            rule,
            reply,
        } => (session_id, ForwardCommand::Start { rule, reply }),
        ControlCommand::Stop {
            session_id,
            id,
            reply,
        } => (session_id, ForwardCommand::Stop { id, reply }),
    };
    let cancelled = match &command {
        ForwardCommand::Start { reply, .. } => reply.is_closed(),
        ForwardCommand::Stop { reply, .. } => reply.is_closed(),
    };
    if cancelled {
        return;
    }
    let rejection = if role != Role::Controller {
        Some("only controllers can create or stop forwarding rules".to_string())
    } else if let Some(session) = sessions.get(&session_id) {
        if session.stop.is_none() {
            Some("forwarding session is stopping".to_string())
        } else {
            match session.forwarding_status.load(Ordering::Acquire) {
                FORWARD_READY => None,
                FORWARD_LEGACY => {
                    Some("peer or signaling server does not support TCP forwarding".to_string())
                }
                _ => Some("forwarding session is not ready".to_string()),
            }
        }
    } else {
        Some("forwarding session is no longer active".to_string())
    };
    let rejection = rejection.or_else(|| match &command {
        ForwardCommand::Start { rule, .. } => validate_forward_rule(rule).err(),
        ForwardCommand::Stop { .. } => None,
    });
    if let Some(message) = rejection {
        reject_forward_command(session_id, command, message, emit);
        return;
    }
    let session = sessions.get(&session_id).expect("validated active session");
    if let Err(error) = session.forwarding.try_send(command) {
        let (command, message) = match error {
            mpsc::error::TrySendError::Full(command) => {
                (command, "forwarding command queue is full")
            }
            mpsc::error::TrySendError::Closed(command) => (command, "forwarding session has ended"),
        };
        reject_forward_command(session_id, command, message.to_string(), emit);
    }
}

fn reject_forward_command(
    session_id: SessionId,
    command: ForwardCommand,
    message: String,
    emit: impl Fn(ClientEvent),
) {
    match command {
        ForwardCommand::Start { rule, reply } => {
            emit(ClientEvent::PortForwardError {
                session_id,
                forward_id: rule.id,
                message: message.clone(),
            });
            let _ = reply.send(Err(message));
        }
        ForwardCommand::Stop { id, reply } => {
            emit(ClientEvent::PortForwardError {
                session_id,
                forward_id: id,
                message: message.clone(),
            });
            let _ = reply.send(Err(message));
        }
    }
}

/// Run until the supplied shutdown future resolves, then release and await all
/// sessions and signaling tasks before returning. Dropping the future also
/// aborts its owned task groups as a last-resort cancellation guard.
pub async fn run_until_shutdown(
    mut cfg: AppConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    tokio::pin!(shutdown);
    if cfg.role == Role::Controller && cfg.peer_id.is_none() {
        bail!("controller requires a --peer id");
    }
    validate_forward_ports(&cfg.allow_forward_ports).map_err(anyhow::Error::msg)?;
    if cfg.role == Role::Controller && !cfg.allow_forward_ports.is_empty() {
        bail!("allowed forwarding ports apply only to the host role");
    }
    if cfg.role == Role::Host && !cfg.initial_forwards.is_empty() {
        bail!("initial forwarding rules apply only to the controller role");
    }
    validate_initial_forwards(&cfg.initial_forwards).map_err(anyhow::Error::msg)?;
    let mut controls = cfg.controls.take();
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
        allow_forward_ports: cfg.allow_forward_ports,
        initial_forwards: cfg.initial_forwards,
    });

    let mut sessions: HashMap<SessionId, SessionControl> = HashMap::new();
    let mut tasks = JoinSet::new();
    let mut connect_sent = false;
    let mut retry_at = None;

    loop {
        let msg = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            command = async {
                match &mut controls {
                    Some(receiver) => receiver.0.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match command {
                    Some(command) => route_control(ctx.role, &sessions, command, |event| ctx.emit(event)),
                    None => controls = None,
                }
                continue;
            }
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
                let (forwarding_handle, forwarding_commands) = forwarding::command_channel();
                let forwarding_status = Arc::new(AtomicU8::new(FORWARD_PENDING));
                sessions.insert(
                    session_id.clone(),
                    SessionControl {
                        messages: tx,
                        stop: Some(stop),
                        forwarding: forwarding_handle.clone(),
                        forwarding_status: forwarding_status.clone(),
                    },
                );
                let ctx = ctx.clone();
                let sid = session_id.clone();
                tasks.spawn(async move {
                    let res = until_stopped(
                        run_session(
                            ctx.clone(),
                            sid.clone(),
                            peer_id,
                            peer_noise_pubkey,
                            rx,
                            SessionForwarding {
                                handle: forwarding_handle,
                                commands: forwarding_commands,
                                status: forwarding_status,
                            },
                        ),
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
    forwarding: SessionForwarding,
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
            tcp_forwarding_v1: true,
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
    // We advertised support above; an old peer or a server that strips the
    // new field safely selects the original RDP wire format on both sides.
    let supports_forwarding = peer_offer.tcp_forwarding_v1;
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

    // 7. New peers multiplex persistent RDP and explicitly permitted TCP
    // forwarding. Closing a local RDP client does not end this session.
    if supports_forwarding {
        return match ctx.role {
            Role::Host => {
                ctx.emit(ClientEvent::HostBridging {
                    session_id: session_id.clone(),
                    rdp_addr: ctx.rdp_addr.to_string(),
                });
                forwarding::serve_host(
                    tunnel,
                    ctx.rdp_addr,
                    ctx.enable_udp,
                    ctx.allow_forward_ports.clone(),
                )
                .await
            }
            Role::Controller => {
                let SessionForwarding {
                    handle,
                    commands,
                    status,
                } = forwarding;
                let (ready, ready_rx) = oneshot::channel();
                let serve = forwarding::serve_controller(
                    tunnel,
                    ctx.listen_addr,
                    ctx.enable_udp,
                    commands,
                    |addr| {
                        info!(%session_id, "RDP tunnel READY — point your RDP client (mstsc) at {addr}");
                        status.store(FORWARD_READY, Ordering::Release);
                        ctx.emit(ClientEvent::PortForwardingAvailable {
                            session_id: session_id.clone(),
                            available: true,
                        });
                        ctx.emit(ClientEvent::RdpReady {
                            session_id: session_id.clone(),
                            listen_addr: addr.to_string(),
                        });
                        let _ = ready.send(());
                    },
                    |event| emit_forward_event(&ctx, &session_id, event),
                );
                let initial = async {
                    if ready_rx.await.is_ok() {
                        for rule in &ctx.initial_forwards {
                            match handle.start(rule.clone()).await {
                                Ok(listen_addr) => {
                                    info!(%session_id, forward_id = %rule.id, %listen_addr, remote_port = rule.remote_port, "TCP forwarding listener ready")
                                }
                                Err(message) => {
                                    warn!(%session_id, forward_id = %rule.id, %message, "initial forwarding rule failed")
                                }
                            }
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                };
                tokio::try_join!(serve, initial).map(|_| ())
            }
        };
    }

    forwarding.status.store(FORWARD_LEGACY, Ordering::Release);
    if ctx.role == Role::Controller {
        ctx.emit(ClientEvent::PortForwardingAvailable {
            session_id: session_id.clone(),
            available: false,
        });
        for rule in &ctx.initial_forwards {
            let message = "peer or signaling server does not support TCP forwarding".to_string();
            warn!(%session_id, forward_id = %rule.id, %message, "initial forwarding rule rejected");
            ctx.emit(ClientEvent::PortForwardError {
                session_id: session_id.clone(),
                forward_id: rule.id.clone(),
                message,
            });
        }
    }
    // Legacy peers retain the original single-RDP-stream behavior.
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

fn emit_forward_event(ctx: &Ctx, session_id: &str, event: ForwardEvent) {
    let session_id = session_id.to_string();
    ctx.emit(match event {
        ForwardEvent::Ready {
            id,
            listen_addr,
            remote_port,
        } => ClientEvent::PortForwardStarted {
            session_id,
            forward_id: id,
            listen_addr: listen_addr.to_string(),
            remote_port,
        },
        ForwardEvent::Stopped { id } => ClientEvent::PortForwardStopped {
            session_id,
            forward_id: id,
        },
        ForwardEvent::Error { id, message } => {
            warn!(%session_id, forward_id = %id, %message, "TCP forwarding error");
            ClientEvent::PortForwardError {
                session_id,
                forward_id: id,
                message,
            }
        }
    });
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

    fn rule(id: &str) -> ForwardRule {
        ForwardRule {
            id: id.to_string(),
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            remote_port: 8080,
        }
    }

    fn controlled_session(status: u8) -> (SessionControl, mpsc::Receiver<ForwardCommand>) {
        let (messages, _) = unbounded_channel();
        let (stop, _) = oneshot::channel();
        let (forwarding, commands) = forwarding::command_channel();
        (
            SessionControl {
                messages,
                stop: Some(stop),
                forwarding,
                forwarding_status: Arc::new(AtomicU8::new(status)),
            },
            commands,
        )
    }

    #[test]
    fn validates_host_allowlist_and_controller_rule_boundaries() {
        assert!(validate_forward_ports(&[]).is_ok());
        assert!(validate_forward_ports(&[22, 8080, 65535]).is_ok());
        assert!(validate_forward_ports(&[0]).is_err());
        assert!(validate_forward_ports(&[22, 22]).is_err());
        assert!(validate_forward_ports(&(1..=129).collect::<Vec<_>>()).is_err());
        assert!(validate_initial_forwards(&[rule("first")]).is_ok());
        assert!(validate_initial_forwards(&[rule("same"), rule("same")]).is_err());
        assert!(validate_initial_forwards(
            &(0..33)
                .map(|n| rule(&format!("rule-{n}")))
                .collect::<Vec<_>>()
        )
        .is_err());
        let mut bad = rule("valid");
        bad.listen_addr = "0.0.0.0:8080".parse().unwrap();
        assert!(validate_forward_rule(&bad).is_err());
        assert!(validate_forward_rule(&rule("../invalid")).is_err());
    }

    #[tokio::test]
    async fn forwarding_commands_reject_stale_host_pending_and_legacy_sessions() {
        for (role, status, id, expected) in [
            (Role::Controller, FORWARD_READY, "stale", "no longer active"),
            (Role::Host, FORWARD_READY, "current", "only controllers"),
            (Role::Controller, FORWARD_PENDING, "current", "not ready"),
            (
                Role::Controller,
                FORWARD_LEGACY,
                "current",
                "does not support",
            ),
        ] {
            let (session, mut commands) = controlled_session(status);
            let mut sessions = HashMap::new();
            sessions.insert("current".into(), session);
            let (reply, response) = oneshot::channel();
            let events = std::cell::RefCell::new(Vec::new());
            route_control(
                role,
                &sessions,
                ControlCommand::Start {
                    session_id: id.into(),
                    rule: rule("rule"),
                    reply,
                },
                |e| events.borrow_mut().push(e),
            );
            assert!(response.await.unwrap().unwrap_err().contains(expected));
            assert!(commands.try_recv().is_err());
            assert!(
                matches!(&events.borrow()[0], ClientEvent::PortForwardError { session_id, .. } if session_id == id)
            );
        }
    }

    #[tokio::test]
    async fn cancelled_commands_are_not_forwarded_and_disconnected_handles_fail() {
        let (session, mut commands) = controlled_session(FORWARD_READY);
        let mut sessions = HashMap::new();
        sessions.insert("current".into(), session);
        let (reply, response) = oneshot::channel();
        drop(response);
        route_control(
            Role::Controller,
            &sessions,
            ControlCommand::Start {
                session_id: "current".into(),
                rule: rule("cancelled"),
                reply,
            },
            |_| panic!("cancelled command emitted an event"),
        );
        assert!(commands.try_recv().is_err());
        let (old_handle, receiver) = control_channel();
        drop(receiver);
        assert!(old_handle
            .start("current".into(), rule("old"))
            .await
            .is_err());
        assert!(old_handle
            .stop("current".into(), "old".into())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn ready_session_commands_are_fifo_and_stopping_sessions_reject_new_rules() {
        let (session, mut commands) = controlled_session(FORWARD_READY);
        let mut sessions = HashMap::new();
        sessions.insert("current".into(), session);
        let (start_reply, start_response) = oneshot::channel();
        let (stop_reply, stop_response) = oneshot::channel();
        route_control(
            Role::Controller,
            &sessions,
            ControlCommand::Start {
                session_id: "current".into(),
                rule: rule("ordered"),
                reply: start_reply,
            },
            |_| {},
        );
        route_control(
            Role::Controller,
            &sessions,
            ControlCommand::Stop {
                session_id: "current".into(),
                id: "ordered".into(),
                reply: stop_reply,
            },
            |_| {},
        );
        let ForwardCommand::Start { rule, reply } = commands.recv().await.unwrap() else {
            panic!("start must precede stop");
        };
        reply.send(Ok(rule.listen_addr)).unwrap();
        assert!(start_response.await.unwrap().is_ok());
        let ForwardCommand::Stop { id, reply } = commands.recv().await.unwrap() else {
            panic!("expected stop");
        };
        assert_eq!(id, "ordered");
        reply.send(Ok(())).unwrap();
        assert!(stop_response.await.unwrap().is_ok());
        sessions.get_mut("current").unwrap().stop = None;
        let (reply, response) = oneshot::channel();
        route_control(
            Role::Controller,
            &sessions,
            ControlCommand::Stop {
                session_id: "current".into(),
                id: "ordered".into(),
                reply,
            },
            |_| {},
        );
        assert!(response.await.unwrap().unwrap_err().contains("stopping"));
        assert!(commands.try_recv().is_err());
    }

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
