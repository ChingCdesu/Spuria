//! Negotiated TCP multiplexing over the existing authenticated tunnel stream.
//!
//! Only a controller opens flows. RDP is a separate target from an explicitly
//! allowed host loopback port. Every listener, flow, reader and writer is owned
//! by the session future; cancelling it closes all of them. Per-flow credits
//! backpressure a slow receiver without blocking unrelated flows. Invalid
//! frames or exhausted protocol queues fail rather than allocate without bound.
//!
//! Wire contract (only after both peers negotiate `tcp_forwarding_v1`): each
//! direction starts with the eight bytes `SPMUX01\n`, then frames consisting of
//! `kind: u8 | stream_id: u32 BE | payload_length: u16 BE | payload`. DATA payloads
//! contain 1..=16384 bytes. Kinds are OPEN=1, DATA=2, FIN=3, RESET=4, OPENED=5,
//! CREDIT=6, PING=7 and PONG=8. Stream zero is reserved for empty PING/PONG;
//! controllers allocate monotonically increasing, nonzero TCP stream IDs.
//!
//! OPEN carries `[0]` for the configured RDP service or `[1, port_hi, port_lo]`
//! for an allowlisted host `127.0.0.1` TCP port. OPENED is empty and precedes any
//! controller DATA. Each direction initially permits eight DATA frames per flow;
//! an empty CREDIT restores one permit only after that DATA is written to the
//! local socket. FIN is empty, ordered after DATA, and half-closes only that
//! direction. RESET carries one reason byte and aborts both flow halves even
//! after FIN. OPEN/OPENED/CREDIT/RESET and heartbeats use a separate bounded
//! control queue; DATA and FIN share a FIFO queue to preserve byte/EOF ordering.

use crate::tunnel::{quic, Tunnel};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use spuria_common::transport::Role;
use std::{
    collections::{HashMap, HashSet},
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, oneshot, Semaphore},
};

pub const MAX_RULES: usize = 32;
pub const MAX_FLOWS: usize = 128;
const MAX_DATA: usize = 16 * 1024;
const FLOW_QUEUE: usize = 16;
const FLOW_WINDOW: usize = 8;
const PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const MAGIC: &[u8; 8] = b"SPMUX01\n";
const OPEN: u8 = 1;
const DATA: u8 = 2;
const FIN: u8 = 3;
const RESET: u8 = 4;
const OPENED: u8 = 5;
const CREDIT: u8 = 6;
const PING: u8 = 7;
const PONG: u8 = 8;
const DENIED: u8 = 1;
const CONNECT_FAILED: u8 = 2;
const LIMIT: u8 = 3;
const SLOW: u8 = 4;
const INVALID: u8 = 5;
const REMOVED: u8 = 6;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForwardRule {
    pub id: String,
    pub listen_addr: SocketAddr,
    pub remote_port: u16,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForwardEvent {
    Ready {
        id: String,
        listen_addr: SocketAddr,
        remote_port: u16,
    },
    Stopped {
        id: String,
    },
    Error {
        id: String,
        message: String,
    },
}

pub enum ForwardCommand {
    Start {
        rule: ForwardRule,
        reply: oneshot::Sender<std::result::Result<SocketAddr, String>>,
    },
    Stop {
        id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
}

#[derive(Clone)]
pub struct ForwardHandle {
    tx: mpsc::Sender<ForwardCommand>,
}

pub fn command_channel() -> (ForwardHandle, mpsc::Receiver<ForwardCommand>) {
    let (tx, rx) = mpsc::channel(32);
    (ForwardHandle { tx }, rx)
}

impl ForwardHandle {
    pub fn try_send(
        &self,
        command: ForwardCommand,
    ) -> std::result::Result<(), mpsc::error::TrySendError<ForwardCommand>> {
        self.tx.try_send(command)
    }

    pub async fn start(&self, rule: ForwardRule) -> std::result::Result<SocketAddr, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ForwardCommand::Start { rule, reply })
            .await
            .map_err(|_| "Forwarding session ended".to_string())?;
        rx.await
            .map_err(|_| "Forwarding session ended".to_string())?
    }

    pub async fn stop(&self, id: String) -> std::result::Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ForwardCommand::Stop { id, reply })
            .await
            .map_err(|_| "Forwarding session ended".to_string())?;
        rx.await
            .map_err(|_| "Forwarding session ended".to_string())?
    }
}

pub fn validate_rule(rule: &ForwardRule) -> std::result::Result<(), String> {
    if rule.id.is_empty()
        || rule.id.len() > 64
        || !rule
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(
            "Forwarding rule id must contain 1 to 64 letters, digits, hyphens or underscores"
                .into(),
        );
    }
    if !rule.listen_addr.ip().is_loopback() {
        return Err("Forwarding listeners must bind a loopback address".into());
    }
    if rule.remote_port == 0 {
        return Err("Remote port must be nonzero".into());
    }
    Ok(())
}

pub async fn serve_controller(
    tunnel: Tunnel,
    rdp_listen: SocketAddr,
    enable_udp: bool,
    commands: mpsc::Receiver<ForwardCommand>,
    on_rdp_ready: impl FnOnce(SocketAddr),
    on_event: impl Fn(ForwardEvent),
) -> Result<()> {
    let datagrams = if enable_udp {
        tunnel.datagram_conn()
    } else {
        None
    };
    let listener = TcpListener::bind(rdp_listen)
        .await
        .context("binding RDP listener")?;
    let addr = listener.local_addr()?;
    let udp = if datagrams.is_some() {
        Some(
            UdpSocket::bind(addr)
                .await
                .context("binding RDP UDP listener")?,
        )
    } else {
        None
    };
    let (bridge, stream) = tokio::io::duplex(64 * 1024);
    let mut listeners = HashMap::new();
    listeners.insert(
        ListenerKey::Rdp,
        Listener {
            socket: listener,
            target: Target::Rdp,
        },
    );
    on_rdp_ready(addr);
    let engine = run_mux(stream, Side::Controller, listeners, commands, on_event);
    let udp = async move {
        if let (Some(conn), Some(socket)) = (datagrams, udp) {
            if let Err(error) = quic::controller_udp_forward_socket(conn, socket).await {
                tracing::warn!(%error, "RDP UDP forwarding stopped; TCP multiplexing remains active");
            }
        }
        std::future::pending::<Result<()>>().await
    };
    tokio::select! {
        r = tunnel.bridge(bridge, Role::Controller) => r,
        r = engine => r,
        r = udp => r,
    }
}

pub async fn serve_host(
    tunnel: Tunnel,
    rdp_addr: SocketAddr,
    enable_udp: bool,
    allowed_ports: Vec<u16>,
) -> Result<()> {
    if allowed_ports.len() > 128 || allowed_ports.contains(&0) {
        bail!("invalid forwarding allowlist");
    }
    let datagrams = if enable_udp {
        tunnel.datagram_conn()
    } else {
        None
    };
    let (bridge, stream) = tokio::io::duplex(64 * 1024);
    let (_, commands) = command_channel();
    let engine = run_mux(
        stream,
        Side::Host {
            rdp_addr,
            allowed: allowed_ports.into_iter().collect(),
        },
        HashMap::new(),
        commands,
        |_| {},
    );
    let udp = async move {
        if let Some(conn) = datagrams {
            if let Err(error) = quic::host_udp_forward(conn, rdp_addr).await {
                tracing::warn!(%error, "RDP UDP forwarding stopped; TCP multiplexing remains active");
            }
        }
        std::future::pending::<Result<()>>().await
    };
    tokio::select! {
        r = tunnel.bridge(bridge, Role::Host) => r,
        r = engine => r,
        r = udp => r,
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ListenerKey {
    Rdp,
    Rule(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Rdp,
    Port(u16),
}
struct Listener {
    socket: TcpListener,
    target: Target,
}
enum Side {
    Controller,
    Host {
        rdp_addr: SocketAddr,
        allowed: HashSet<u16>,
    },
}
struct Flow {
    incoming: mpsc::Sender<Frame>,
    stop: oneshot::Sender<()>,
    rule: Option<String>,
    credits: Arc<Semaphore>,
}
type FlowTasks = FuturesUnordered<BoxFuture<'static, (u32, Result<()>)>>;

#[derive(Debug)]
struct Frame {
    kind: u8,
    id: u32,
    payload: Vec<u8>,
}
impl Frame {
    fn empty(kind: u8, id: u32) -> Self {
        Self {
            kind,
            id,
            payload: Vec::new(),
        }
    }
    fn reset(id: u32, reason: u8) -> Self {
        Self {
            kind: RESET,
            id,
            payload: vec![reason],
        }
    }
    fn open(id: u32, target: Target) -> Self {
        let payload = match target {
            Target::Rdp => vec![0],
            Target::Port(p) => {
                let [a, b] = p.to_be_bytes();
                vec![1, a, b]
            }
        };
        Self {
            kind: OPEN,
            id,
            payload,
        }
    }
    fn target(&self) -> Result<Target> {
        match self.payload.as_slice() {
            [0] => Ok(Target::Rdp),
            [1, a, b] if *a != 0 || *b != 0 => Ok(Target::Port(u16::from_be_bytes([*a, *b]))),
            _ => bail!("invalid multiplex target"),
        }
    }
}

#[derive(Clone)]
struct Outbound {
    control: mpsc::Sender<Frame>,
    data: mpsc::Sender<Frame>,
}
impl Outbound {
    fn control(&self, frame: Frame) -> Result<()> {
        self.control
            .try_send(frame)
            .map_err(|_| anyhow!("multiplex control queue exhausted or closed"))
    }
    async fn data(&self, frame: Frame) -> Result<()> {
        self.data
            .send(frame)
            .await
            .map_err(|_| anyhow!("multiplex writer closed"))
    }
    async fn credit(&self, id: u32) -> Result<()> {
        self.control
            .send(Frame::empty(CREDIT, id))
            .await
            .map_err(|_| anyhow!("multiplex writer closed"))
    }
}

// This future is polled for its entire lifetime, never recreated inside the
// actor's select. Partial read_exact progress cannot be lost to a command.
async fn reader<R: AsyncRead + Unpin>(mut read: R, incoming: mpsc::Sender<Frame>) -> Result<()> {
    let mut magic = [0u8; 8];
    tokio::time::timeout(Duration::from_secs(10), read.read_exact(&mut magic))
        .await
        .context("multiplex handshake timed out")??;
    if &magic != MAGIC {
        bail!("invalid multiplex handshake");
    }
    loop {
        let frame = read_frame(&mut read).await?;
        if incoming.send(frame).await.is_err() {
            return Ok(());
        }
    }
}

async fn read_frame<R: AsyncRead + Unpin>(read: &mut R) -> Result<Frame> {
    let mut header = [0u8; 7];
    read.read_exact(&mut header).await?;
    let kind = header[0];
    let id = u32::from_be_bytes(header[1..5].try_into().unwrap());
    let length = u16::from_be_bytes([header[5], header[6]]) as usize;
    let valid_length = match kind {
        OPEN => length == 1 || length == 3,
        DATA => length > 0 && length <= MAX_DATA,
        FIN | OPENED | CREDIT | PING | PONG => length == 0,
        RESET => length == 1,
        _ => false,
    };
    if (id == 0) != matches!(kind, PING | PONG) || !valid_length {
        bail!("invalid multiplex frame header");
    }
    let mut payload = vec![0; length];
    read.read_exact(&mut payload).await?;
    let frame = Frame { kind, id, payload };
    if kind == OPEN {
        frame.target()?;
    }
    Ok(frame)
}

async fn write_frame<W: AsyncWrite + Unpin>(write: &mut W, frame: Frame) -> Result<()> {
    let mut header = [0u8; 7];
    header[0] = frame.kind;
    header[1..5].copy_from_slice(&frame.id.to_be_bytes());
    header[5..7].copy_from_slice(&(frame.payload.len() as u16).to_be_bytes());
    write.write_all(&header).await?;
    write.write_all(&frame.payload).await?;
    Ok(())
}

async fn writer<W: AsyncWrite + Unpin>(
    mut write: W,
    mut control: mpsc::Receiver<Frame>,
    mut data: mpsc::Receiver<Frame>,
) -> Result<()> {
    write.write_all(MAGIC).await?;
    loop {
        let frame = tokio::select! {
            biased;
            Some(frame) = control.recv() => frame,
            Some(frame) = data.recv() => frame,
            else => return Ok(()),
        };
        write_frame(&mut write, frame).await?;
    }
}

async fn accept_any(
    listeners: &HashMap<ListenerKey, Listener>,
) -> Result<(TcpStream, ListenerKey, Target)> {
    let pending = FuturesUnordered::new();
    for (key, listener) in listeners {
        pending.push(async move {
            listener
                .socket
                .accept()
                .await
                .map(|(stream, _)| (stream, key.clone(), listener.target))
        });
    }
    tokio::pin!(pending);
    match pending.next().await {
        Some(result) => Ok(result?),
        None => std::future::pending().await,
    }
}

async fn run_mux<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    side: Side,
    listeners: HashMap<ListenerKey, Listener>,
    commands: mpsc::Receiver<ForwardCommand>,
    on_event: impl Fn(ForwardEvent),
) -> Result<()> {
    let (read, write) = tokio::io::split(stream);
    let (incoming_tx, incoming_rx) = mpsc::channel(64);
    let (control_tx, control_rx) = mpsc::channel(MAX_FLOWS * (FLOW_WINDOW + 4));
    let (data_tx, data_rx) = mpsc::channel(64);
    let outbound = Outbound {
        control: control_tx,
        data: data_tx,
    };
    // Both pumps and the actor are scoped futures. A session drop synchronously
    // drops all flow sockets, regardless of queue or network backpressure.
    tokio::select! {
        r = reader(read, incoming_tx) => r,
        r = writer(write, control_rx, data_rx) => r,
        r = actor(side, listeners, commands, incoming_rx, outbound, on_event) => r,
    }
}

async fn actor(
    side: Side,
    mut listeners: HashMap<ListenerKey, Listener>,
    mut commands: mpsc::Receiver<ForwardCommand>,
    mut incoming: mpsc::Receiver<Frame>,
    outbound: Outbound,
    on_event: impl Fn(ForwardEvent),
) -> Result<()> {
    let mut flows: HashMap<u32, Flow> = HashMap::new();
    let mut tasks = FlowTasks::new();
    let mut next_id = 1u32;
    let mut highest_open = 0u32;
    let mut commands_open = true;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_peer_activity = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if peer_expired(last_peer_activity, tokio::time::Instant::now()) { bail!("TCP multiplex peer stopped responding"); }
                outbound.control(Frame::empty(PING, 0))?;
            }
            Some((id, result)) = tasks.next(), if !tasks.is_empty() => {
                finish_flow(id, result, &mut flows, &outbound, &on_event)?;
            }
            accepted = accept_any(&listeners), if matches!(side, Side::Controller) => {
                let (stream, key, target) = accepted?;
                let rule = match key { ListenerKey::Rdp => None, ListenerKey::Rule(id) => Some(id) };
                if tasks.len() >= MAX_FLOWS || next_id == u32::MAX {
                    if let Some(id) = rule { on_event(ForwardEvent::Error { id, message: "Session TCP flow limit reached".into() }); }
                    drop(stream);
                    continue;
                }
                let id = next_id;
                next_id += 1;
                // Enqueue Opens in monotonically increasing id order before
                // polling flow work. The peer can reject replayed stream ids.
                outbound.control(Frame::open(id, target))?;
                let (flow, task) = make_flow(id, rule, stream, true, outbound.clone());
                flows.insert(id, flow); tasks.push(task);
            }
            command = commands.recv(), if commands_open && matches!(side, Side::Controller) => {
                match command {
                    None => commands_open = false,
                    Some(ForwardCommand::Start { rule, reply }) => {
                        if reply.is_closed() { continue; }
                        let result = bind_rule(&rule, &listeners).await;
                        match result {
                            Ok((listener, addr)) => {
                                // Dropped caller = dropped listener, never an orphan rule.
                                if reply.send(Ok(addr)).is_ok() {
                                    listeners.insert(ListenerKey::Rule(rule.id.clone()), listener);
                                    on_event(ForwardEvent::Ready { id: rule.id, listen_addr: addr, remote_port: rule.remote_port });
                                }
                            }
                            Err(message) => {
                                let _ = reply.send(Err(message.clone()));
                                on_event(ForwardEvent::Error { id: rule.id, message });
                            }
                        }
                    }
                    Some(ForwardCommand::Stop { id, reply }) => {
                        let found = listeners.remove(&ListenerKey::Rule(id.clone())).is_some();
                        let removed: Vec<u32> = flows.iter().filter(|(_, flow)| flow.rule.as_deref() == Some(id.as_str())).map(|(&id, _)| id).collect();
                        let mut waiting: HashSet<u32> = removed.iter().copied().collect();
                        for id in removed {
                            if let Some(flow) = flows.remove(&id) { let _ = flow.stop.send(()); }
                            outbound.control(Frame::reset(id, REMOVED))?;
                        }
                        // Poll the cancelled scoped futures until their sockets
                        // are gone before acknowledging removal.
                        while !waiting.is_empty() {
                            if let Some((done, result)) = tasks.next().await {
                                waiting.remove(&done);
                                finish_flow(done, result, &mut flows, &outbound, &on_event)?;
                            } else { break; }
                        }
                        if found {
                            on_event(ForwardEvent::Stopped { id });
                            let _ = reply.send(Ok(()));
                        } else {
                            let message = "Forwarding rule not found".to_string();
                            on_event(ForwardEvent::Error { id, message: message.clone() });
                            let _ = reply.send(Err(message));
                        }
                    }
                }
            }
            Some(frame) = incoming.recv() => {
                last_peer_activity = tokio::time::Instant::now();
                let id = frame.id;
                if frame.kind == PING { outbound.control(Frame::empty(PONG, 0))?; continue; }
                if frame.kind == PONG { continue; }
                if frame.kind == RESET {
                    reset_flow(id, frame.payload[0], &mut flows, &on_event);
                    continue;
                }
                if frame.kind == CREDIT {
                    if let Some(flow) = flows.get(&id) {
                        if flow.credits.available_permits() >= FLOW_WINDOW {
                            let flow = flows.remove(&id).unwrap();
                            let _ = flow.stop.send(());
                            outbound.control(Frame::reset(id, INVALID))?;
                            if let Some(id) = flow.rule { on_event(ForwardEvent::Error { id, message: "Invalid TCP flow credit received".into() }); }
                        } else { flow.credits.add_permits(1); }
                    }
                    continue;
                }
                if frame.kind == OPEN {
                    let Side::Host { rdp_addr, allowed } = &side else { bail!("host may not open multiplex flows"); };
                    if id <= highest_open { outbound.control(Frame::reset(id, INVALID))?; continue; }
                    highest_open = id;
                    if tasks.len() >= MAX_FLOWS { outbound.control(Frame::reset(id, LIMIT))?; continue; }
                    let target = match frame.target()? {
                        Target::Rdp => *rdp_addr,
                        Target::Port(port) if allowed.contains(&port) => SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                        Target::Port(_) => { outbound.control(Frame::reset(id, DENIED))?; continue; }
                    };
                    let (tx, rx) = mpsc::channel(FLOW_QUEUE);
                    let (stop, stopped) = oneshot::channel();
                    let credits = Arc::new(Semaphore::new(FLOW_WINDOW));
                    flows.insert(id, Flow { incoming: tx, stop, rule: None, credits: credits.clone() });
                    let send = outbound.clone();
                    tasks.push(async move {
                        let work = async {
                            let connection = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(target)).await;
                            let stream = match connection {
                                Ok(Ok(stream)) => stream,
                                _ => { send.control(Frame::reset(id, CONNECT_FAILED))?; bail!("Remote loopback service connection failed"); }
                            };
                            send.control(Frame::empty(OPENED, id))?;
                            bridge_flow(stream, id, rx, send.clone(), credits).await
                        };
                        let result = tokio::select! { biased; _ = stopped => Ok(()), r = work => r };
                        (id, result)
                    }.boxed());
                } else if let Some(flow) = flows.get(&id) {
                    if flow.incoming.try_send(frame).is_err() {
                        let flow = flows.remove(&id).unwrap();
                        let _ = flow.stop.send(());
                        outbound.control(Frame::reset(id, SLOW))?;
                        if let Some(id) = flow.rule { on_event(ForwardEvent::Error { id, message: "Slow TCP flow closed because its receive queue is full".into() }); }
                    }
                }
                // Late Data/FIN/RESET for an already removed flow is ignored.
            }
        }
    }
}

async fn bind_rule(
    rule: &ForwardRule,
    listeners: &HashMap<ListenerKey, Listener>,
) -> std::result::Result<(Listener, SocketAddr), String> {
    validate_rule(rule)?;
    if listeners.contains_key(&ListenerKey::Rule(rule.id.clone())) {
        return Err("Forwarding rule id already exists".into());
    }
    if listeners.len().saturating_sub(1) >= MAX_RULES {
        return Err("Forwarding rule limit reached".into());
    }
    let socket = TcpListener::bind(rule.listen_addr)
        .await
        .map_err(|_| "Local forwarding address is unavailable or already in use".to_string())?;
    let addr = socket
        .local_addr()
        .map_err(|_| "Could not read forwarding listener address".to_string())?;
    Ok((
        Listener {
            socket,
            target: Target::Port(rule.remote_port),
        },
        addr,
    ))
}

fn make_flow<S>(
    id: u32,
    rule: Option<String>,
    stream: S,
    wait_open: bool,
    outbound: Outbound,
) -> (Flow, BoxFuture<'static, (u32, Result<()>)>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<Frame>(FLOW_QUEUE);
    let (stop, stopped) = oneshot::channel();
    let credits = Arc::new(Semaphore::new(FLOW_WINDOW));
    let send_credits = credits.clone();
    let task = async move {
        let work = async {
            if wait_open {
                let frame = tokio::time::timeout(Duration::from_secs(15), rx.recv())
                    .await
                    .context("Remote TCP open timed out")?
                    .ok_or_else(|| anyhow!("TCP flow closed before acknowledgement"))?;
                if frame.kind == RESET {
                    bail!("{}", reset_message(frame.payload[0]));
                }
                if frame.kind != OPENED {
                    bail!("Expected TCP open acknowledgement");
                }
            }
            bridge_flow(stream, id, rx, outbound, send_credits).await
        };
        let result = tokio::select! { biased; _ = stopped => Ok(()), r = work => r };
        (id, result)
    }
    .boxed();
    (
        Flow {
            incoming: tx,
            stop,
            rule,
            credits,
        },
        task,
    )
}

fn finish_flow(
    id: u32,
    result: Result<()>,
    flows: &mut HashMap<u32, Flow>,
    outbound: &Outbound,
    on_event: &impl Fn(ForwardEvent),
) -> Result<()> {
    if let Some(flow) = flows.remove(&id) {
        if let Err(error) = result {
            outbound.control(Frame::reset(id, INVALID))?;
            if let Some(id) = flow.rule {
                on_event(ForwardEvent::Error {
                    id,
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

fn reset_flow(
    id: u32,
    reason: u8,
    flows: &mut HashMap<u32, Flow>,
    on_event: &impl Fn(ForwardEvent),
) {
    if let Some(flow) = flows.remove(&id) {
        let _ = flow.stop.send(());
        if let Some(id) = flow.rule {
            on_event(ForwardEvent::Error {
                id,
                message: reset_message(reason).into(),
            });
        }
    }
}

fn reset_message(reason: u8) -> &'static str {
    match reason {
        DENIED => "Remote host does not allow this TCP port",
        CONNECT_FAILED => "Remote loopback service connection failed",
        LIMIT => "Remote session TCP flow limit reached",
        SLOW => "Remote TCP flow receive queue is full",
        REMOVED => "TCP forwarding rule was removed",
        _ => "TCP flow was closed by the peer",
    }
}

fn peer_expired(last_activity: tokio::time::Instant, now: tokio::time::Instant) -> bool {
    now.saturating_duration_since(last_activity) >= PEER_IDLE_TIMEOUT
}

async fn bridge_flow<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    id: u32,
    mut incoming: mpsc::Receiver<Frame>,
    outbound: Outbound,
    credits: Arc<Semaphore>,
) -> Result<()> {
    let (mut read, mut write) = tokio::io::split(stream);
    let up = async {
        let mut bytes = vec![0; MAX_DATA];
        loop {
            let n = read.read(&mut bytes).await?;
            if n == 0 {
                outbound.data(Frame::empty(FIN, id)).await?;
                return Ok::<(), anyhow::Error>(());
            }
            credits
                .acquire()
                .await
                .map_err(|_| anyhow!("TCP flow closed"))?
                .forget();
            outbound
                .data(Frame {
                    kind: DATA,
                    id,
                    payload: bytes[..n].to_vec(),
                })
                .await?;
        }
    };
    let down = async {
        while let Some(frame) = incoming.recv().await {
            match frame.kind {
                DATA => {
                    write.write_all(&frame.payload).await?;
                    outbound.credit(id).await?;
                }
                FIN => {
                    write.shutdown().await?;
                    return Ok::<(), anyhow::Error>(());
                }
                RESET => bail!("{}", reset_message(frame.payload[0])),
                _ => bail!("Unexpected TCP flow frame"),
            }
        }
        bail!("TCP flow channel closed")
    };
    tokio::try_join!(up, down)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::JoinSet;

    struct Fixture {
        handle: ForwardHandle,
        rdp: SocketAddr,
        events: mpsc::UnboundedReceiver<ForwardEvent>,
        tasks: JoinSet<()>,
    }

    // Module-only fixtures: the encrypted transport is replaced by an in-memory
    // duplex. No signaling/relay servers, application sessions or RDP clients.
    async fn fixture(rdp_addr: SocketAddr, allowed_ports: Vec<u16>) -> Fixture {
        let (controller, host) = tokio::io::duplex(4096);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rdp = listener.local_addr().unwrap();
        let mut listeners = HashMap::new();
        listeners.insert(
            ListenerKey::Rdp,
            Listener {
                socket: listener,
                target: Target::Rdp,
            },
        );
        let (handle, commands) = command_channel();
        let (_, host_commands) = command_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _ = run_mux(controller, Side::Controller, listeners, commands, |event| {
                let _ = event_tx.send(event);
            })
            .await;
        });
        tasks.spawn(async move {
            let _ = run_mux(
                host,
                Side::Host {
                    rdp_addr,
                    allowed: allowed_ports.into_iter().collect(),
                },
                HashMap::new(),
                host_commands,
                |_| {},
            )
            .await;
        });
        Fixture {
            handle,
            rdp,
            events,
            tasks,
        }
    }

    async fn echo_service(tasks: &mut JoinSet<()>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tasks.spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let Ok((mut stream, _)) = result else { break; };
                        clients.spawn(async move {
                            let (mut read, mut write) = stream.split();
                            let _ = tokio::io::copy(&mut read, &mut write).await;
                            let _ = write.shutdown().await;
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        addr
    }

    fn rule(id: &str, port: u16) -> ForwardRule {
        ForwardRule {
            id: id.into(),
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            remote_port: port,
        }
    }

    async fn echo_once(addr: SocketAddr, payload: &[u8]) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(payload).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, payload);
    }

    #[test]
    fn rules_reject_public_binds_invalid_ports_and_ids() {
        let mut value = rule("valid_1-A", 443);
        assert!(validate_rule(&value).is_ok());
        value.listen_addr = "0.0.0.0:8080".parse().unwrap();
        assert!(validate_rule(&value).is_err());
        value.listen_addr = "[::1]:0".parse().unwrap();
        value.remote_port = 0;
        assert!(validate_rule(&value).is_err());
        value.remote_port = 443;
        for id in ["", "a/b", "a b", "a\n", "非ASCII"] {
            value.id = id.into();
            assert!(validate_rule(&value).is_err());
        }
        value.id = "x".repeat(65);
        assert!(validate_rule(&value).is_err());
    }

    #[test]
    fn heartbeat_deadline_starts_with_real_peer_activity() {
        let start = tokio::time::Instant::now();
        assert!(!peer_expired(start, start));
        assert!(!peer_expired(start, start + Duration::from_secs(89)));
        assert!(peer_expired(start, start + Duration::from_secs(90)));
        let refreshed = start + Duration::from_secs(60);
        assert!(!peer_expired(refreshed, start + Duration::from_secs(90)));
    }

    #[tokio::test]
    async fn malformed_frame_is_rejected_without_reading_claimed_payload() {
        for header in [
            [DATA, 0, 0, 0, 1, 255, 255],
            [FIN, 0, 0, 0, 1, 0, 1],
            [OPENED, 0, 0, 0, 0, 0, 0],
            [PING, 0, 0, 0, 1, 0, 0],
        ] {
            let (mut peer, mut wire) = tokio::io::duplex(16);
            peer.write_all(&header).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), read_frame(&mut wire))
                    .await
                    .unwrap()
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn concurrent_and_repeated_tcp_flows_survive_denied_port() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut services = JoinSet::new();
            let rdp_service = echo_service(&mut services).await;
            let allowed_service = echo_service(&mut services).await;
            let mut fixture = fixture(rdp_service, vec![allowed_service.port()]).await;
            let forwarded = fixture
                .handle
                .start(rule("allowed", allowed_service.port()))
                .await
                .unwrap();
            // Selecting Port(rdp_port) must not bypass the allowlist, even though
            // the separately negotiated Rdp target can access that service.
            let denied = fixture
                .handle
                .start(rule("denied", rdp_service.port()))
                .await
                .unwrap();
            let mut rejected = TcpStream::connect(denied).await.unwrap();
            let mut byte = [0u8; 1];
            assert!(matches!(rejected.read(&mut byte).await, Ok(0) | Err(_)));
            let mut saw_denial = false;
            while let Some(event) = fixture.events.recv().await {
                if let ForwardEvent::Error { id, message } = event {
                    if id == "denied" {
                        assert!(message.contains("does not allow"));
                        saw_denial = true;
                        break;
                    }
                }
            }
            assert!(saw_denial);
            tokio::join!(
                echo_once(forwarded, b"port one"),
                echo_once(forwarded, b"port two"),
                echo_once(fixture.rdp, b"rdp")
            );
            echo_once(fixture.rdp, b"repeated rdp").await;
            echo_once(forwarded, b"forward remains after rdp EOF").await;
            fixture.tasks.shutdown().await;
            services.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn large_transfer_to_slow_consumer_uses_credit_without_truncation() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut services = JoinSet::new();
            let service = echo_service(&mut services).await;
            let mut fixture = fixture(service, vec![service.port()]).await;
            let addr = fixture
                .handle
                .start(rule("bulk", service.port()))
                .await
                .unwrap();
            let stream = TcpStream::connect(addr).await.unwrap();
            let (mut read, mut write) = stream.into_split();
            let payload = vec![0x5a; 8 * 1024 * 1024];
            let expected_length = payload.len();
            let sender = async {
                write.write_all(&payload).await.unwrap();
                write.shutdown().await.unwrap();
            };
            let receiver = async {
                let mut total = 0;
                let mut buffer = vec![0; 64 * 1024];
                loop {
                    let n = read.read(&mut buffer).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    assert!(buffer[..n].iter().all(|&byte| byte == 0x5a));
                    total += n;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                assert_eq!(total, expected_length);
            };
            tokio::join!(
                sender,
                receiver,
                echo_once(fixture.rdp, b"responsive unrelated RDP flow")
            );
            while let Ok(event) = fixture.events.try_recv() {
                assert!(!matches!(event, ForwardEvent::Error { .. }));
            }
            fixture.tasks.shutdown().await;
            services.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn removal_closes_active_flows_and_releases_listener_before_reply() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut services = JoinSet::new();
            let service = echo_service(&mut services).await;
            let mut fixture = fixture(service, vec![service.port()]).await;
            let addr = fixture
                .handle
                .start(rule("temporary", service.port()))
                .await
                .unwrap();
            let mut first = TcpStream::connect(addr).await.unwrap();
            let mut second = TcpStream::connect(addr).await.unwrap();
            for stream in [&mut first, &mut second] {
                stream.write_all(b"x").await.unwrap();
                assert_eq!(stream.read_u8().await.unwrap(), b'x');
            }
            fixture.handle.stop("temporary".into()).await.unwrap();
            let _replacement = TcpListener::bind(addr).await.unwrap();
            for stream in [&mut first, &mut second] {
                let mut byte = [0];
                assert!(matches!(stream.read(&mut byte).await, Ok(0) | Err(_)));
            }
            echo_once(fixture.rdp, b"rdp survives removal").await;
            fixture.tasks.shutdown().await;
            services.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn half_closed_request_can_receive_late_response() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = server.local_addr().unwrap();
            let mut fixture = fixture(target, vec![]).await;
            let server_work = async {
                let (mut stream, _) = server.accept().await.unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).await.unwrap();
                assert_eq!(request, b"request");
                stream.write_all(b"response after EOF").await.unwrap();
                stream.shutdown().await.unwrap();
            };
            let client_work = async {
                let mut stream = TcpStream::connect(fixture.rdp).await.unwrap();
                stream.write_all(b"request").await.unwrap();
                stream.shutdown().await.unwrap();
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, b"response after EOF");
            };
            tokio::join!(server_work, client_work);
            fixture.tasks.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cancelled_start_and_conflicting_rules_do_not_leave_listeners() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut fixture = fixture("127.0.0.1:9".parse().unwrap(), vec![]).await;
            let first = fixture.handle.start(rule("one", 443)).await.unwrap();
            assert!(fixture.handle.start(rule("one", 443)).await.is_err());
            let mut duplicate_addr = rule("two", 443);
            duplicate_addr.listen_addr = first;
            assert!(fixture.handle.start(duplicate_addr).await.is_err());
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = probe.local_addr().unwrap();
            drop(probe);
            let mut cancelled_rule = rule("cancelled", 443);
            cancelled_rule.listen_addr = addr;
            let (reply, receiver) = oneshot::channel();
            drop(receiver);
            fixture
                .handle
                .try_send(ForwardCommand::Start {
                    rule: cancelled_rule,
                    reply,
                })
                .unwrap_or_else(|_| panic!("command queue unexpectedly full"));
            // FIFO command completion establishes that the cancelled Start was processed.
            fixture.handle.stop("one".into()).await.unwrap();
            let _rebound = TcpListener::bind(addr).await.unwrap();
            fixture.tasks.shutdown().await;
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn partial_frame_read_does_not_block_commands_or_corrupt_next_frame() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (stream, mut peer) = tokio::io::duplex(4096);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut listeners = HashMap::new();
            listeners.insert(
                ListenerKey::Rdp,
                Listener {
                    socket: listener,
                    target: Target::Rdp,
                },
            );
            let (handle, commands) = command_channel();
            let mut tasks = JoinSet::new();
            tasks.spawn(async move {
                let _ = run_mux(stream, Side::Controller, listeners, commands, |_| {}).await;
            });
            peer.write_all(MAGIC).await.unwrap();
            peer.write_all(&[PING, 0, 0]).await.unwrap();
            let addr = handle.start(rule("during-header", 443)).await.unwrap();
            assert!(TcpListener::bind(addr).await.is_err());
            peer.write_all(&[0, 0, 0, 0]).await.unwrap();
            let mut magic = [0; 8];
            peer.read_exact(&mut magic).await.unwrap();
            assert_eq!(&magic, MAGIC);
            loop {
                if read_frame(&mut peer).await.unwrap().kind == PONG {
                    break;
                }
            }
            handle.stop("during-header".into()).await.unwrap();
            tasks.shutdown().await;
            let _rebound = TcpListener::bind(addr).await.unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reset_after_peer_fin_closes_the_remaining_local_half() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (stream, mut local) = tokio::io::duplex(32);
            let (control, _control_rx) = mpsc::channel(16);
            let (data, _data_rx) = mpsc::channel(16);
            let (flow, task) = make_flow(1, None, stream, false, Outbound { control, data });
            let sender = flow.incoming.clone();
            let mut flows = HashMap::from([(1, flow)]);
            let (finished, completion) = oneshot::channel();
            let drive_flow = async {
                let (id, result) = task.await;
                assert_eq!(id, 1);
                result.unwrap();
                let _ = finished.send(());
            };
            let peer = async {
                sender.send(Frame::empty(FIN, 1)).await.unwrap();
                assert_eq!(local.read(&mut [0u8; 1]).await.unwrap(), 0);
                // Its receive half has completed, but its send half is still
                // waiting for local input. RESET must cancel that half directly
                // instead of enqueueing a message nobody will receive.
                reset_flow(1, REMOVED, &mut flows, &|_| {});
                completion.await.unwrap();
                assert!(flows.is_empty());
                assert!(local.write_all(b"must not remain open").await.is_err());
            };
            tokio::join!(drive_flow, peer);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn silent_transport_eof_releases_all_listeners_and_active_flow_sockets() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (stream, mut peer) = tokio::io::duplex(4096);
            let rdp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let rdp_addr = rdp_listener.local_addr().unwrap();
            let listeners = HashMap::from([(
                ListenerKey::Rdp,
                Listener {
                    socket: rdp_listener,
                    target: Target::Rdp,
                },
            )]);
            let (handle, commands) = command_channel();
            let mut tasks = JoinSet::new();
            tasks.spawn(async move {
                run_mux(stream, Side::Controller, listeners, commands, |_| {}).await
            });
            peer.write_all(MAGIC).await.unwrap();
            let forwarded = handle.start(rule("eof", 443)).await.unwrap();
            let mut local = TcpStream::connect(forwarded).await.unwrap();
            let mut magic = [0u8; 8];
            peer.read_exact(&mut magic).await.unwrap();
            assert_eq!(&magic, MAGIC);
            let id = loop {
                let frame = read_frame(&mut peer).await.unwrap();
                if frame.kind == OPEN {
                    break frame.id;
                }
            };
            write_frame(&mut peer, Frame::empty(OPENED, id))
                .await
                .unwrap();
            write_frame(
                &mut peer,
                Frame {
                    kind: DATA,
                    id,
                    payload: b"x".to_vec(),
                },
            )
            .await
            .unwrap();
            assert_eq!(local.read_u8().await.unwrap(), b'x');
            // No FIN, RESET or shutdown command: only the underlying encrypted
            // stream disappearing must synchronously release owned resources.
            drop(peer);
            let _ = tasks.join_next().await.unwrap().unwrap();
            let _rdp_rebound = TcpListener::bind(rdp_addr).await.unwrap();
            let _forward_rebound = TcpListener::bind(forwarded).await.unwrap();
            assert!(matches!(local.read(&mut [0u8; 1]).await, Ok(0) | Err(_)));
            assert!(handle.start(rule("late", 443)).await.is_err());
        })
        .await
        .unwrap();
    }
}
