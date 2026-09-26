//! WebSocket client to the signaling server: register, pump messages, heartbeat.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use spuria_common::{
    ids::DeviceId,
    protocol::{ClientMsg, ServerMsg},
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinSet;
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
use tracing::{debug, warn};

/// Information sent in the initial Register frame.
pub struct RegisterInfo {
    pub device_id: DeviceId,
    pub noise_pubkey: String,
    pub secret: String,
    pub display_name: Option<String>,
}

/// Cloneable handle for sending control messages to the signaling server.
#[derive(Clone)]
pub struct SignalingHandle {
    tx: UnboundedSender<ClientMsg>,
}

impl SignalingHandle {
    pub fn send(&self, msg: ClientMsg) {
        if self.tx.send(msg).is_err() {
            warn!("signaling send on closed channel");
        }
    }
}

/// Owns the signaling pump. Dropping this owner aborts the pump; explicit
/// shutdown also waits for the socket and heartbeat to be released.
pub struct SignalingConnection {
    pub handle: SignalingHandle,
    pub inbound: UnboundedReceiver<ServerMsg>,
    tasks: JoinSet<()>,
}

impl SignalingConnection {
    pub async fn shutdown(&mut self) {
        self.tasks.shutdown().await;
    }
}

/// Connect, register, and start one owned read/write/heartbeat pump.
pub async fn connect(url: &str, reg: RegisterInfo) -> Result<SignalingConnection> {
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("websocket connect to {url}"))?;
    // The very first frame is Register.
    let register = ClientMsg::Register {
        device_id: reg.device_id,
        noise_pubkey: reg.noise_pubkey,
        secret: reg.secret,
        display_name: reg.display_name,
    };
    ws.send(Message::text(register.to_text()?))
        .await
        .context("sending registration")?;
    start_pump(ws)
}

fn start_pump<S>(ws: WebSocketStream<S>) -> Result<SignalingConnection>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut write, mut read) = ws.split();
    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();
    let (in_tx, in_rx) = unbounded_channel::<ServerMsg>();
    let heartbeat_frame = ClientMsg::Heartbeat.to_text()?;

    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                // UI/session shutdown must close the socket even if no frames
                // arrive and handles have been cloned by callers.
                _ = in_tx.closed() => break,
                msg = out_rx.recv() => {
                    let Some(msg) = msg else { break; };
                    match msg.to_text() {
                        Ok(text) => {
                            if write.send(Message::text(text)).await.is_err() { break; }
                        }
                        Err(e) => warn!(error = %e, "failed to serialize client msg"),
                    }
                }
                frame = read.next() => match frame {
                    Some(Ok(Message::Text(t))) => match ServerMsg::from_text(t.as_str()) {
                        Ok(sm) => { if in_tx.send(sm).is_err() { break; } }
                        Err(e) => debug!(error = %e, "ignoring malformed server msg"),
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        debug!(error = %e, "ws read error");
                        break;
                    }
                },
                _ = heartbeat.tick() => {
                    if write.send(Message::text(heartbeat_frame.clone())).await.is_err() {
                        break;
                    }
                }
            }
        }
        // Drop both halves together. There is no detached heartbeat retaining
        // the outbound channel after the reader exits.
    });

    Ok(SignalingConnection {
        handle: SignalingHandle { tx: out_tx },
        inbound: in_rx,
        tasks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn pair() -> (
        SignalingConnection,
        WebSocketStream<tokio::io::DuplexStream>,
    ) {
        // In-memory WebSocket frames only: no signaling server, tunnels, RDP,
        // network listeners, or end-to-end application flow are exercised.
        let (client, server) = tokio::io::duplex(4096);
        let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        (start_pump(client).unwrap(), server)
    }

    #[tokio::test]
    async fn remote_close_stops_writer_and_heartbeat_with_reader() {
        let (mut connection, mut server) = pair().await;
        server.send(Message::Close(None)).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), connection.inbound.recv())
                .await
                .unwrap()
                .is_none()
        );
        connection.tasks.join_next().await.unwrap().unwrap();
        assert!(connection.handle.tx.is_closed());
        assert!(connection.tasks.is_empty());
    }

    #[tokio::test]
    async fn shutdown_waits_for_signaling_resources_even_with_cloned_handle() {
        let (mut connection, _server) = pair().await;
        let handle = connection.handle.clone();
        connection.shutdown().await;
        assert!(connection.tasks.is_empty());
        assert!(handle.tx.is_closed());
        assert!(connection.inbound.recv().await.is_none());
    }
}
