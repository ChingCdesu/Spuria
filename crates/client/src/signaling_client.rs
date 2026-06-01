//! WebSocket client to the signaling server: register, pump messages, heartbeat.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use spuria_common::{
    ids::DeviceId,
    protocol::{ClientMsg, ServerMsg},
};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;
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

/// Connect, register, and start the read/write/heartbeat pumps. Returns a
/// handle for sending and a receiver of inbound server messages.
pub async fn connect(
    url: &str,
    reg: RegisterInfo,
) -> Result<(SignalingHandle, UnboundedReceiver<ServerMsg>)> {
    let (ws, _resp) = tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("websocket connect to {url}"))?;
    let (mut write, mut read) = ws.split();

    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();
    let (in_tx, in_rx) = unbounded_channel::<ServerMsg>();

    // The very first frame is Register.
    let _ = out_tx.send(ClientMsg::Register {
        device_id: reg.device_id,
        noise_pubkey: reg.noise_pubkey,
        secret: reg.secret,
        display_name: reg.display_name,
    });

    // Outbound pump.
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match msg.to_text() {
                Ok(text) => {
                    if write.send(Message::text(text)).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!(error = %e, "failed to serialize client msg"),
            }
        }
        let _ = write.close().await;
    });

    // Inbound pump.
    tokio::spawn(async move {
        while let Some(frame) = read.next().await {
            match frame {
                Ok(Message::Text(t)) => match ServerMsg::from_text(t.as_str()) {
                    Ok(sm) => {
                        if in_tx.send(sm).is_err() {
                            break;
                        }
                    }
                    Err(e) => debug!(error = %e, "ignoring malformed server msg"),
                },
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    debug!(error = %e, "ws read error");
                    break;
                }
            }
        }
    });

    // Heartbeat pump.
    let hb = out_tx.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(15));
        loop {
            iv.tick().await;
            if hb.send(ClientMsg::Heartbeat).is_err() {
                break;
            }
        }
    });

    Ok((SignalingHandle { tx: out_tx }, in_rx))
}
