//! Authenticated, bounded relay rendezvous and blind ciphertext forwarding.

use anyhow::{bail, Context, Result};
use spuria_common::{
    protocol::relay::RELAY_MAGIC,
    ratelimit::RateLimiter,
    relay_ticket::{unix_now, RelayTicketKey, MAX_TICKET_LEN},
};
use std::{
    collections::{hash_map::Entry, HashMap},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{watch, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::Instant,
};
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct RelayConfig {
    pub bind: SocketAddr,
    /// Dedicated ticket key shared only with signaling (at least 32 bytes).
    pub relay_secret: String,
    pub park_timeout: Duration,
    pub hello_timeout: Duration,
    /// Close paired connections after this much time without byte progress.
    pub idle_timeout: Duration,
    pub max_sessions: u64,
    /// Includes connections reading hello, parked connections, and both peers
    /// of active sessions. Each accepted socket owns a permit until dropped.
    pub max_connections: usize,
    /// Replay records are retained until ticket expiry, including failed and
    /// completed sessions. At capacity new tickets are rejected, never evicted.
    pub max_ticket_records: usize,
    pub max_conn_per_sec: f64,
}

struct Arrival {
    stream: TcpStream,
    peer: SocketAddr,
    _permit: OwnedSemaphorePermit,
}

enum TicketState<T> {
    Waiting { arrival: T, since: Instant },
    Consumed,
}

struct TicketRecord<T> {
    expires_at: u64,
    state: TicketState<T>,
}

/// Pairing and replay consumption are atomic. Pending sockets and replay
/// tombstones share a bounded table, so neither can evade admission.
struct Rendezvous<T> {
    records: HashMap<String, TicketRecord<T>>,
    max_records: usize,
}

impl<T> Rendezvous<T> {
    fn new(max_records: usize) -> Self {
        Self {
            records: HashMap::new(),
            max_records,
        }
    }

    fn arrive(
        &mut self,
        ticket: String,
        expires_at: u64,
        arrival: T,
        now_unix: u64,
        now: Instant,
        park_timeout: Duration,
    ) -> Result<Option<(T, T)>> {
        if expires_at <= now_unix {
            bail!("relay ticket expired");
        }
        if self.records.len() >= self.max_records && !self.records.contains_key(&ticket) {
            bail!("relay ticket admission capacity reached");
        }
        match self.records.entry(ticket) {
            Entry::Vacant(entry) => {
                entry.insert(TicketRecord {
                    expires_at,
                    state: TicketState::Waiting {
                        arrival,
                        since: now,
                    },
                });
                Ok(None)
            }
            Entry::Occupied(mut entry) => {
                match std::mem::replace(&mut entry.get_mut().state, TicketState::Consumed) {
                    TicketState::Waiting {
                        arrival: partner,
                        since,
                    } if now.duration_since(since) < park_timeout => Ok(Some((arrival, partner))),
                    TicketState::Waiting { .. } => bail!("relay partner wait expired"),
                    TicketState::Consumed => bail!("relay ticket already consumed"),
                }
            }
        }
    }

    fn prune(&mut self, now_unix: u64, now: Instant, park_timeout: Duration) {
        self.records.retain(|_, record| {
            if record.expires_at <= now_unix {
                return false;
            }
            if matches!(&record.state, TicketState::Waiting { since, .. }
                if now.duration_since(*since) >= park_timeout)
            {
                // Drop the socket/permit but retain its tombstone to expiry.
                record.state = TicketState::Consumed;
            }
            true
        });
    }
}

#[derive(Clone)]
struct State {
    rendezvous: Arc<Mutex<Rendezvous<Arrival>>>,
    sessions: Arc<Semaphore>,
    key: Arc<RelayTicketKey>,
    cfg: Arc<RelayConfig>,
    clock: Arc<AtomicU64>,
}

impl State {
    fn new(cfg: RelayConfig) -> Result<Self> {
        let key = RelayTicketKey::new(&cfg.relay_secret)?;
        if cfg.max_sessions == 0
            || cfg.max_sessions > Semaphore::MAX_PERMITS as u64
            || cfg.max_connections == 0
            || cfg.max_connections > Semaphore::MAX_PERMITS
            || cfg.max_ticket_records == 0
            || cfg.hello_timeout.is_zero()
            || cfg.park_timeout.is_zero()
            || cfg.idle_timeout.is_zero()
        {
            bail!("relay limits and timeouts must be positive and within semaphore limits");
        }
        Ok(Self {
            rendezvous: Arc::new(Mutex::new(Rendezvous::new(cfg.max_ticket_records))),
            sessions: Arc::new(Semaphore::new(cfg.max_sessions as usize)),
            key: Arc::new(key),
            cfg: Arc::new(cfg),
            clock: Arc::new(AtomicU64::new(unix_now()?)),
        })
    }

    fn now_unix(&self) -> Result<u64> {
        Ok(observe_time(&self.clock, unix_now()?))
    }
}

/// Fail closed during wall-clock rollback rather than revive expired tickets
/// whose replay tombstones have already been pruned.
fn observe_time(clock: &AtomicU64, now: u64) -> u64 {
    clock.fetch_max(now, Ordering::Relaxed).max(now)
}

pub async fn run(cfg: RelayConfig) -> Result<()> {
    let state = State::new(cfg)?;
    let listener = TcpListener::bind(state.cfg.bind)
        .await
        .with_context(|| format!("binding relay on {}", state.cfg.bind))?;
    info!(addr = %state.cfg.bind, "relay listening");
    let connections = Arc::new(Semaphore::new(state.cfg.max_connections));
    let limiter = RateLimiter::new(
        (state.cfg.max_conn_per_sec * 3.0).max(10.0),
        state.cfg.max_conn_per_sec,
    );
    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => { warn!(%error, "relay accept failed"); continue; }
                };
                if !limiter.allow(peer.ip()) { continue; }
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    debug!(%peer, "relay total connection limit reached");
                    continue;
                };
                let state = state.clone();
                tasks.spawn(async move {
                    if let Err(error) = handle_conn(state, stream, peer, permit).await {
                        debug!(%peer, %error, "relay connection ended");
                    }
                });
            }
            _ = tick.tick() => {
                if let Ok(now_unix) = state.now_unix() {
                    state.rendezvous.lock().unwrap()
                        .prune(now_unix, Instant::now(), state.cfg.park_timeout);
                }
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}

async fn handle_conn(
    state: State,
    mut stream: TcpStream,
    peer: SocketAddr,
    permit: OwnedSemaphorePermit,
) -> Result<()> {
    let ticket = read_hello_with_timeout(&mut stream, state.cfg.hello_timeout).await?;
    let claims = state
        .key
        .verify_at(&ticket, state.now_unix()?)
        .context("verifying relay admission")?;
    let arrival = Arrival {
        stream,
        peer,
        _permit: permit,
    };
    let pair = state.rendezvous.lock().unwrap().arrive(
        ticket,
        claims.expires_at,
        arrival,
        state.now_unix()?,
        Instant::now(),
        state.cfg.park_timeout,
    )?;
    if let Some((a, b)) = pair {
        let _session_permit = state
            .sessions
            .clone()
            .try_acquire_owned()
            .context("relay active session limit reached")?;
        // Retain both Arrival values (and socket permits) through the splice.
        let (mut a, mut b) = (a, b);
        info!(a = %a.peer, b = %b.peer, "relay session up");
        let (ab, ba) =
            copy_with_idle_timeout(&mut a.stream, &mut b.stream, state.cfg.idle_timeout).await?;
        info!(a = %a.peer, b = %b.peer, to_b = ab, to_a = ba, "relay session closed");
    }
    Ok(())
}

async fn read_hello_with_timeout<R: AsyncRead + Unpin>(
    stream: &mut R,
    timeout: Duration,
) -> Result<String> {
    tokio::time::timeout(timeout, read_hello(stream))
        .await
        .context("relay hello deadline exceeded")?
}

async fn read_hello<R: AsyncRead + Unpin>(stream: &mut R) -> Result<String> {
    let mut magic = [0u8; 16];
    stream.read_exact(&mut magic).await?;
    if &magic != RELAY_MAGIC {
        bail!("bad relay magic");
    }
    let len = stream.read_u16().await? as usize;
    if len == 0 || len > MAX_TICKET_LEN {
        bail!("invalid ticket length {len}");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    String::from_utf8(buf).context("ticket not utf-8")
}

/// Both directions share an activity clock. Progress on either direction keeps
/// a half-closed or asymmetric stream alive; stalled writes still time out.
async fn copy_with_idle_timeout<A, B>(a: &mut A, b: &mut B, idle: Duration) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let (activity, mut observed) = watch::channel(Instant::now());
    let copy = async {
        tokio::try_join!(
            copy_direction(ar, bw, activity.clone()),
            copy_direction(br, aw, activity)
        )
    };
    let deadline = async {
        loop {
            let at = *observed.borrow_and_update() + idle;
            tokio::select! {
                biased;
                changed = observed.changed() => if changed.is_err() { std::future::pending::<()>().await; },
                _ = tokio::time::sleep_until(at) => return,
            }
        }
    };
    tokio::select! {
        result = copy => Ok(result?),
        _ = deadline => bail!("relay session idle deadline exceeded"),
    }
}

async fn copy_direction<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut read: R,
    mut write: W,
    activity: watch::Sender<Instant>,
) -> std::io::Result<u64> {
    let mut buf = [0u8; 8192];
    let mut total = 0;
    loop {
        let n = read.read(&mut buf).await?;
        if n == 0 {
            write.shutdown().await?;
            return Ok(total);
        }
        activity.send_replace(Instant::now());
        let mut remaining = &buf[..n];
        while !remaining.is_empty() {
            let written = write.write(remaining).await?;
            if written == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            total += written as u64;
            remaining = &remaining[written..];
            activity.send_replace(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spuria_common::protocol::relay::hello;

    #[tokio::test]
    async fn hello_parsing_and_deadline() {
        let (mut a, mut b) = tokio::io::duplex(256);
        a.write_all(&hello("my-ticket")).await.unwrap();
        assert_eq!(read_hello(&mut b).await.unwrap(), "my-ticket");
        a.write_all(&RELAY_MAGIC[..4]).await.unwrap();
        assert!(read_hello_with_timeout(&mut b, Duration::from_millis(20))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn read_hello_rejects_bad_magic_and_excessive_length() {
        let (mut a, mut b) = tokio::io::duplex(256);
        a.write_all(b"NOT-SPURIA-MAGIC").await.unwrap();
        assert!(read_hello(&mut b).await.is_err());
        a.write_all(RELAY_MAGIC).await.unwrap();
        a.write_u16((MAX_TICKET_LEN + 1) as u16).await.unwrap();
        assert!(read_hello(&mut b).await.is_err());
    }

    #[test]
    fn a_ticket_pairs_once_and_replay_tombstones_are_bounded() {
        let mut table = Rendezvous::new(1);
        let now = Instant::now();
        let wait = Duration::from_secs(30);
        assert!(table
            .arrive("one".into(), 120, 1, 1, now, wait)
            .unwrap()
            .is_none());
        assert_eq!(
            table.arrive("one".into(), 120, 2, 1, now, wait).unwrap(),
            Some((2, 1))
        );
        assert!(table.arrive("one".into(), 120, 3, 1, now, wait).is_err());
        assert!(table.arrive("two".into(), 121, 4, 1, now, wait).is_err());
        table.prune(119, now, wait);
        assert_eq!(table.records.len(), 1);
        assert!(table.arrive("two".into(), 121, 4, 119, now, wait).is_err());
        table.prune(120, now, wait);
        assert!(table.records.is_empty());
        assert!(table.arrive("one".into(), 120, 5, 120, now, wait).is_err());
        assert!(table
            .arrive("two".into(), 121, 6, 120, now, wait)
            .unwrap()
            .is_none());
    }

    #[test]
    fn wall_clock_rollback_cannot_revive_pruned_tickets() {
        let mut table = Rendezvous::new(1);
        let clock = AtomicU64::new(100);
        let now = Instant::now();
        let wait = Duration::from_secs(30);
        table
            .arrive("old".into(), 120, 1, observe_time(&clock, 100), now, wait)
            .unwrap();
        table.prune(observe_time(&clock, 120), now, wait);
        assert!(table.records.is_empty());
        let rolled_back = observe_time(&clock, 100);
        assert_eq!(rolled_back, 120);
        assert!(table
            .arrive("old".into(), 120, 2, rolled_back, now, wait)
            .is_err());
    }

    #[test]
    fn parked_timeout_burns_ticket_and_releases_connection_permit() {
        let mut table = Rendezvous::new(2);
        let connections = Arc::new(Semaphore::new(1));
        let now = Instant::now();
        let wait = Duration::from_secs(30);
        let permit = connections.clone().try_acquire_owned().unwrap();
        table
            .arrive("one".into(), 120, permit, 1, now, wait)
            .unwrap();
        assert!(connections.clone().try_acquire_owned().is_err());
        table.prune(31, now + wait, wait);
        assert_eq!(connections.available_permits(), 1);
        let permit = connections.clone().try_acquire_owned().unwrap();
        assert!(table
            .arrive("one".into(), 120, permit, 31, now + wait, wait)
            .is_err());
        assert_eq!(connections.available_permits(), 1);
        assert_eq!(table.records.len(), 1);
    }

    #[tokio::test]
    async fn idle_splice_times_out_and_active_permit_is_released_on_abort() {
        let (_a_peer, mut a) = tokio::io::duplex(128);
        let (_b_peer, mut b) = tokio::io::duplex(128);
        assert!(
            copy_with_idle_timeout(&mut a, &mut b, Duration::from_millis(20))
                .await
                .is_err()
        );
        let sessions = Arc::new(Semaphore::new(1));
        let permit = sessions.clone().try_acquire_owned().unwrap();
        let task = tokio::spawn(async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        });
        assert!(sessions.clone().try_acquire_owned().is_err());
        task.abort();
        let _ = task.await;
        assert_eq!(sessions.available_permits(), 1);
    }

    #[tokio::test]
    async fn splice_preserves_bytes_and_half_close() {
        let (mut a_peer, mut a) = tokio::io::duplex(128);
        let (mut b_peer, mut b) = tokio::io::duplex(128);
        let copying = tokio::spawn(async move {
            copy_with_idle_timeout(&mut a, &mut b, Duration::from_secs(2)).await
        });
        a_peer.write_all(b"request").await.unwrap();
        a_peer.shutdown().await.unwrap();
        let mut request = Vec::new();
        b_peer.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");
        b_peer.write_all(b"response").await.unwrap();
        b_peer.shutdown().await.unwrap();
        let mut response = Vec::new();
        a_peer.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response");
        assert_eq!(copying.await.unwrap().unwrap(), (7, 8));
    }
}
