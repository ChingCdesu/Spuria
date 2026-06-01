//! UDP reflection service — our minimal STUN substitute (plan §3.1 "内置 STUN
//! 能力"). A client sends [`reflect::REFLECT_MAGIC`] from the very UDP socket it
//! intends to punch with; we echo back the source address we observed, which
//! becomes the client's server-reflexive (srflx) candidate.

use anyhow::{Context, Result};
use spuria_common::protocol::reflect;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tracing::{debug, info};

pub async fn run(bind: SocketAddr) -> Result<()> {
    let sock = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("binding reflect UDP on {bind}"))?;
    info!(addr = %bind, "reflect (srflx) service listening");

    let mut buf = [0u8; 64];
    loop {
        let (n, src) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "reflect recv error");
                continue;
            }
        };
        if reflect::is_request(&buf[..n]) {
            let reply = reflect::response(src);
            if let Err(e) = sock.send_to(&reply, src).await {
                debug!(%src, error = %e, "reflect send error");
            } else {
                debug!(%src, "reflected srflx address");
            }
        }
    }
}
