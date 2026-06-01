//! Relay data plane (plan §3.2). Pairs two TCP connections that present the
//! same ticket and blindly splices their byte streams. The relay only ever
//! sees Noise ciphertext.

use anyhow::{bail, Context, Result};
use dashmap::{mapref::entry::Entry, DashMap};
use spuria_common::protocol::relay::RELAY_MAGIC;
use spuria_common::ratelimit::RateLimiter;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, info, warn};

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub bind: SocketAddr,
    /// How long a first arrival waits to be paired before being evicted.
    pub park_timeout: Duration,
    /// Maximum number of concurrently spliced sessions.
    pub max_sessions: u64,
    /// Sustained new-connection rate allowed per source IP (burst = 3x).
    pub max_conn_per_sec: f64,
}

/// A connection parked waiting for its partner.
struct Parked {
    stream: TcpStream,
    peer: SocketAddr,
    since: Instant,
}

#[derive(Clone)]
struct State {
    pending: Arc<DashMap<String, Parked>>,
    active: Arc<AtomicU64>,
    limiter: Arc<RateLimiter>,
    cfg: Arc<RelayConfig>,
}

pub async fn run(cfg: RelayConfig) -> Result<()> {
    let listener = TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding relay on {}", cfg.bind))?;
    info!(addr = %cfg.bind, "relay listening");

    let limiter = Arc::new(RateLimiter::new(
        (cfg.max_conn_per_sec * 3.0).max(10.0),
        cfg.max_conn_per_sec,
    ));
    let state = State {
        pending: Arc::new(DashMap::new()),
        active: Arc::new(AtomicU64::new(0)),
        limiter,
        cfg: Arc::new(cfg),
    };

    // Janitor: evict parked connections that were never claimed.
    {
        let state = state.clone();
        tokio::spawn(async move { janitor(state).await });
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        if !state.limiter.allow(peer.ip()) {
            debug!(%peer, "connection rate limited");
            continue;
        }
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(state, stream, peer).await {
                debug!(%peer, error = %e, "relay connection ended");
            }
        });
    }
}

async fn handle_conn(state: State, mut stream: TcpStream, peer: SocketAddr) -> Result<()> {
    let ticket = read_hello(&mut stream).await.context("reading hello")?;
    debug!(%peer, ticket = %short(&ticket), "relay hello");

    match state.pending.entry(ticket.clone()) {
        Entry::Occupied(e) => {
            // Partner already waiting — claim it and splice.
            let partner = e.remove();
            splice(&state, ticket, stream, peer, partner).await
        }
        Entry::Vacant(e) => {
            // We are first; park ourselves for the partner to claim.
            e.insert(Parked {
                stream,
                peer,
                since: Instant::now(),
            });
            debug!(%peer, "parked, awaiting partner");
            Ok(())
        }
    }
}

async fn splice(
    state: &State,
    ticket: String,
    mut a: TcpStream,
    a_peer: SocketAddr,
    partner: Parked,
) -> Result<()> {
    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    if active > state.cfg.max_sessions {
        state.active.fetch_sub(1, Ordering::SeqCst);
        bail!(
            "max sessions ({}) reached, dropping",
            state.cfg.max_sessions
        );
    }
    let mut b = partner.stream;
    info!(
        ticket = %short(&ticket),
        a = %a_peer, b = %partner.peer,
        active,
        "relay session up"
    );

    let res = tokio::io::copy_bidirectional(&mut a, &mut b).await;
    state.active.fetch_sub(1, Ordering::SeqCst);
    match res {
        Ok((ab, ba)) => {
            info!(ticket = %short(&ticket), to_b = ab, to_a = ba, "relay session closed");
        }
        Err(e) => debug!(ticket = %short(&ticket), error = %e, "relay splice error"),
    }
    Ok(())
}

/// Read and validate the rendezvous hello, returning the ticket.
async fn read_hello<R: AsyncReadExt + Unpin>(stream: &mut R) -> Result<String> {
    let mut magic = [0u8; 16];
    stream.read_exact(&mut magic).await?;
    if &magic != RELAY_MAGIC {
        bail!("bad relay magic");
    }
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await?;
    let len = u16::from_be_bytes(len) as usize;
    if len == 0 || len > 256 {
        bail!("invalid ticket length {len}");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    String::from_utf8(buf).context("ticket not utf-8")
}

async fn janitor(state: State) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tick.tick().await;
        let now = Instant::now();
        let timeout = state.cfg.park_timeout;
        // Collect stale keys first (don't hold the iterator across the await).
        let stale: Vec<String> = state
            .pending
            .iter()
            .filter(|e| now.duration_since(e.value().since) >= timeout)
            .map(|e| e.key().clone())
            .collect();
        for t in stale {
            if let Some((_, mut p)) = state.pending.remove(&t) {
                let _ = p.stream.shutdown().await; // best-effort
                debug!(ticket = %short(&t), peer = %p.peer, "evicted stale parked connection");
            }
        }
    }
}

/// Abbreviate a ticket for logging (never log the whole secret).
fn short(ticket: &str) -> String {
    ticket.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spuria_common::protocol::relay::hello;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn read_hello_parses_ticket() {
        let (mut a, mut b) = tokio::io::duplex(256);
        a.write_all(&hello("my-ticket")).await.unwrap();
        drop(a);
        assert_eq!(read_hello(&mut b).await.unwrap(), "my-ticket");
    }

    #[tokio::test]
    async fn read_hello_rejects_bad_magic() {
        let (mut a, mut b) = tokio::io::duplex(256);
        a.write_all(b"NOT-SPURIA-MAGIC").await.unwrap(); // 16 bytes, wrong
        a.write_all(&[0, 3, 1, 2, 3]).await.unwrap();
        drop(a);
        assert!(read_hello(&mut b).await.is_err());
    }

    #[test]
    fn short_truncates() {
        assert_eq!(short("0123456789abcdef"), "01234567");
    }
}
