//! Relay tunnel: TCP to the relay server, end-to-end encrypted with Noise KK
//! (plan §2, §4.2). The relay only sees length-prefixed ciphertext.
//!
//! Wire framing after the rendezvous hello: every chunk is `u16 BE len ||
//! ciphertext`. Both the Noise handshake messages and the transport-mode
//! frames use this prefix so the relay can splice opaque bytes.

use anyhow::{bail, Context, Result};
use spuria_common::{
    crypto::{DeviceKey, Handshake, NoiseSession, MAX_PLAINTEXT_CHUNK},
    protocol::{relay::hello, Ticket},
    transport::Role,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Plaintext read size per encrypted frame (kept under [`MAX_PLAINTEXT_CHUNK`]).
const PUMP_CHUNK: usize = 16 * 1024;

pub struct RelayTunnel {
    tcp: TcpStream,
    session: NoiseSession,
}

/// Connect to the relay, present the ticket, and run the Noise KK handshake.
pub async fn connect(
    relay_addr: SocketAddr,
    ticket: &Ticket,
    role: Role,
    local_key: &DeviceKey,
    peer_pubkey: &[u8],
) -> Result<RelayTunnel> {
    let mut tcp = TcpStream::connect(relay_addr)
        .await
        .with_context(|| format!("connecting to relay {relay_addr}"))?;
    tcp.write_all(&hello(ticket))
        .await
        .context("sending relay hello")?;

    let mut hs = if role.is_initiator() {
        Handshake::initiator(local_key, peer_pubkey)?
    } else {
        Handshake::responder(local_key, peer_pubkey)?
    };

    // KK: initiator -> (e, es, ss); responder <- (e, ee, se).
    if role.is_initiator() {
        let m1 = hs.write()?;
        write_framed(&mut tcp, &m1).await?;
        let m2 = read_framed(&mut tcp).await?;
        hs.read(&m2)?;
    } else {
        let m1 = read_framed(&mut tcp).await?;
        hs.read(&m1)?;
        let m2 = hs.write()?;
        write_framed(&mut tcp, &m2).await?;
    }

    if !hs.is_finished() {
        bail!("noise handshake did not complete");
    }
    Ok(RelayTunnel {
        tcp,
        session: hs.into_session()?,
    })
}

impl RelayTunnel {
    pub async fn bridge<S>(self, local: S, _role: Role) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (mut relay_r, mut relay_w) = self.tcp.into_split();
        let (mut local_r, mut local_w) = tokio::io::split(local);
        // Crypto ops are sync and fast; the lock is never held across an await.
        let session = Arc::new(Mutex::new(self.session));

        let up_session = session.clone();
        let up = async move {
            let mut buf = vec![0u8; PUMP_CHUNK];
            loop {
                let n = local_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let frame = {
                    let mut s = up_session.lock().unwrap();
                    s.encrypt_frame(&buf[..n])
                        .map_err(|e| anyhow::anyhow!("{e}"))?
                };
                relay_w.write_all(&frame).await?;
            }
            let _ = relay_w.shutdown().await;
            Ok::<_, anyhow::Error>(())
        };

        let down_session = session.clone();
        let down = async move {
            loop {
                let mut len = [0u8; 2];
                match relay_r.read_exact(&mut len).await {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                }
                let len = u16::from_be_bytes(len) as usize;
                let mut ct = vec![0u8; len];
                relay_r.read_exact(&mut ct).await?;
                let pt = {
                    let mut s = down_session.lock().unwrap();
                    s.decrypt(&ct).map_err(|e| anyhow::anyhow!("{e}"))?
                };
                local_w.write_all(&pt).await?;
            }
            let _ = local_w.shutdown().await;
            Ok::<_, anyhow::Error>(())
        };

        tokio::try_join!(up, down)?;
        Ok(())
    }
}

async fn write_framed<W: AsyncWriteExt + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    if data.len() > MAX_PLAINTEXT_CHUNK + 16 {
        bail!("handshake frame too large");
    }
    w.write_all(&(data.len() as u16).to_be_bytes()).await?;
    w.write_all(data).await?;
    Ok(())
}

async fn read_framed<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let len = u16::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framed_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(2048);
        let payload = vec![9u8; 500];
        write_framed(&mut a, &payload).await.unwrap();
        assert_eq!(read_framed(&mut b).await.unwrap(), payload);
    }

    #[tokio::test]
    async fn framed_empty() {
        let (mut a, mut b) = tokio::io::duplex(64);
        write_framed(&mut a, &[]).await.unwrap();
        assert!(read_framed(&mut b).await.unwrap().is_empty());
    }
}
