//! In-memory device/online table and session bookkeeping (plan §3.1).
//!
//! All routing decisions live here. The server's connection task only parses
//! frames and calls into the registry; the registry decides what to send and
//! to whom. State is in-memory (DashMap); a Redis backend is a future swap
//! (plan §2 "会话状态").
//!
//! The registry also backs the admin API: it exposes read-only snapshots
//! ([`DeviceInfo`]/[`SessionInfo`]), a [`Registry::kick`] control, and a ring
//! buffer audit log.

use dashmap::DashMap;
use rand::RngCore;
use serde::Serialize;
use spuria_common::{
    candidate::SessionOffer,
    ids::DeviceId,
    protocol::{ErrorCode, ServerMsg, SessionId, Ticket},
    relay_ticket::RelayTicketKey,
    transport::TunnelPath,
};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tracing::{debug, info, warn};

/// Maximum audit entries retained in memory.
const AUDIT_CAPACITY: usize = 2000;

/// Identifies one WebSocket registration, even when the device ID is reused.
pub type ConnectionId = u64;

/// A registered, online device.
struct Device {
    connection_id: ConnectionId,
    noise_pubkey: String,
    tx: UnboundedSender<ServerMsg>,
    /// Fires to force the connection's read loop to close (admin kick).
    kick: Option<oneshot::Sender<()>>,
    connected_at: Instant,
    last_seen: Instant,
}

/// A pairing in progress / established between two devices.
struct Session {
    controller: DeviceId,
    host: DeviceId,
    ticket: Option<Ticket>,
    controller_path: Option<TunnelPath>,
    host_path: Option<TunnelPath>,
    created_at: Instant,
}

impl Session {
    fn path(&self) -> Option<TunnelPath> {
        self.controller_path
            .filter(|path| self.host_path == Some(*path))
    }

    /// The other participant relative to `me`.
    fn peer_of(&self, me: &DeviceId) -> Option<&DeviceId> {
        if me == &self.controller {
            Some(&self.host)
        } else if me == &self.host {
            Some(&self.controller)
        } else {
            None
        }
    }
}

/// Read-only device view for the admin API.
#[derive(Clone, Serialize)]
pub struct DeviceInfo {
    pub device_id: String,
    /// Truncated public key (identity hint, not the full key).
    pub noise_pubkey: String,
    pub uptime_secs: u64,
    pub idle_secs: u64,
}

/// Read-only session view for the admin API.
#[derive(Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub controller: String,
    pub host: String,
    pub relayed: bool,
    /// Established path reported by both participants, or unknown/pending.
    pub path: Option<TunnelPath>,
    pub age_secs: u64,
}

/// One audit-log record.
#[derive(Clone, Serialize)]
pub struct AuditEntry {
    pub ts_ms: u64,
    pub kind: String,
    pub detail: String,
}

pub struct Registry {
    devices: DashMap<DeviceId, Device>,
    sessions: DashMap<SessionId, Session>,
    audit: Mutex<VecDeque<AuditEntry>>,
    relay_addr: SocketAddr,
    relay_key: RelayTicketKey,
    next_connection_id: AtomicU64,
    /// Serialize registration replacement, cleanup, and authenticated dispatch.
    lifecycle: Mutex<()>,
}

impl Registry {
    pub fn new(relay_addr: SocketAddr, relay_secret: &str) -> spuria_common::Result<Self> {
        Ok(Self {
            devices: DashMap::new(),
            sessions: DashMap::new(),
            audit: Mutex::new(VecDeque::with_capacity(256)),
            relay_addr,
            relay_key: RelayTicketKey::new(relay_secret)?,
            next_connection_id: AtomicU64::new(1),
            lifecycle: Mutex::new(()),
        })
    }

    pub fn online_count(&self) -> usize {
        self.devices.len()
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Register (or replace) a device. `kick` fires to force-close the socket.
    pub fn register(
        &self,
        id: DeviceId,
        noise_pubkey: String,
        tx: UnboundedSender<ServerMsg>,
        kick: oneshot::Sender<()>,
    ) -> ConnectionId {
        let _guard = self.lifecycle.lock().unwrap();
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let replaced = self.devices.insert(
            id.clone(),
            Device {
                connection_id,
                noise_pubkey,
                tx,
                kick: Some(kick),
                connected_at: now,
                last_seen: now,
            },
        );
        if let Some(mut previous) = replaced {
            debug!(%id, "replacing existing registration");
            if let Some(kick) = previous.kick.take() {
                let _ = kick.send(());
            }
            self.remove_device_sessions(&id);
        }
        info!(%id, online = self.devices.len(), "device registered");
        self.audit("register", id.as_str());
        connection_id
    }

    /// Run one message only while its registration is still current. Holding
    /// the lifecycle guard prevents replacement between validation and action.
    pub fn with_connection(
        &self,
        id: &DeviceId,
        connection_id: ConnectionId,
        action: impl FnOnce(),
    ) {
        let _guard = self.lifecycle.lock().unwrap();
        if self
            .devices
            .get(id)
            .is_some_and(|d| d.connection_id == connection_id)
        {
            action();
        }
    }

    pub fn touch(&self, id: &DeviceId) {
        if let Some(mut d) = self.devices.get_mut(id) {
            d.last_seen = Instant::now();
        }
    }

    /// Remove a device and tear down any sessions it was part of, notifying the
    /// surviving peer.
    pub fn unregister(&self, id: &DeviceId, connection_id: ConnectionId) {
        let _guard = self.lifecycle.lock().unwrap();
        if self
            .devices
            .remove_if(id, |_, d| d.connection_id == connection_id)
            .is_none()
        {
            return;
        }
        info!(%id, online = self.devices.len(), "device unregistered");
        self.audit("unregister", id.as_str());
        self.remove_device_sessions(id);
    }

    fn remove_device_sessions(&self, id: &DeviceId) {
        let affected: Vec<(SessionId, DeviceId)> = self
            .sessions
            .iter()
            .filter_map(|e| e.value().peer_of(id).map(|p| (e.key().clone(), p.clone())))
            .collect();
        for (sid, peer) in affected {
            self.sessions.remove(&sid);
            self.send(&peer, ServerMsg::PeerOffline { session_id: sid });
        }
    }

    /// Force-disconnect a device (admin action). Returns whether it was online.
    pub fn kick(&self, id: &DeviceId) -> bool {
        let fired = self
            .devices
            .get_mut(id)
            .and_then(|mut d| d.kick.take())
            .map(|k| {
                let _ = k.send(());
            })
            .is_some();
        if fired {
            info!(%id, "device kicked by admin");
            self.audit("kick", id.as_str());
        }
        fired
    }

    /// Send a message to a device if it is online.
    pub fn send(&self, to: &DeviceId, msg: ServerMsg) {
        let tx = self.devices.get(to).map(|d| d.tx.clone());
        match tx {
            Some(tx) => {
                if tx.send(msg).is_err() {
                    warn!(%to, "device channel closed while sending");
                }
            }
            None => debug!(%to, "drop message: device offline"),
        }
    }

    fn pubkey_of(&self, id: &DeviceId) -> Option<String> {
        self.devices.get(id).map(|d| d.noise_pubkey.clone())
    }

    /// Controller requests a connection to `host`. Mints a session and notifies
    /// both peers to start punching. Sends an error back to the controller if
    /// the host is offline.
    pub fn start_session(&self, controller: &DeviceId, host: &DeviceId) {
        let (Some(controller_pk), Some(host_pk)) =
            (self.pubkey_of(controller), self.pubkey_of(host))
        else {
            self.send(
                controller,
                ServerMsg::Error {
                    code: ErrorCode::PeerOffline,
                    message: format!("peer {host} is offline"),
                },
            );
            return;
        };

        let session_id = rand_token(8);
        self.sessions.insert(
            session_id.clone(),
            Session {
                controller: controller.clone(),
                host: host.clone(),
                ticket: None,
                controller_path: None,
                host_path: None,
                created_at: Instant::now(),
            },
        );
        info!(%session_id, %controller, %host, "session created");
        self.audit("session", &format!("{controller} -> {host}"));

        // Controller is the initiator; host accepts.
        self.send(
            controller,
            ServerMsg::Punch {
                session_id: session_id.clone(),
                peer_id: host.clone(),
                peer_noise_pubkey: host_pk,
                initiator: true,
            },
        );
        self.send(
            host,
            ServerMsg::Punch {
                session_id,
                peer_id: controller.clone(),
                peer_noise_pubkey: controller_pk,
                initiator: false,
            },
        );
    }

    /// Forward one peer's candidate offer to the other.
    pub fn forward_candidates(&self, from: &DeviceId, session_id: &SessionId, offer: SessionOffer) {
        let Some(peer) = self
            .sessions
            .get(session_id)
            .and_then(|s| s.peer_of(from).cloned())
        else {
            debug!(%session_id, %from, "candidates for unknown session/peer");
            return;
        };
        self.send(
            &peer,
            ServerMsg::Candidates {
                session_id: session_id.clone(),
                offer,
            },
        );
    }

    /// Allocate a relay ticket for the session (idempotent) and notify both
    /// peers.
    pub fn assign_relay(&self, requester: &DeviceId, session_id: &SessionId) {
        let Some(mut s) = self.sessions.get_mut(session_id) else {
            debug!(%session_id, "relay request for unknown session");
            return;
        };
        if s.peer_of(requester).is_none() {
            warn!(%session_id, %requester, "relay request from non-participant");
            return;
        }
        let ticket = match &s.ticket {
            Some(ticket) => ticket.clone(),
            None => match self.relay_key.issue() {
                Ok(ticket) => {
                    s.ticket = Some(ticket.clone());
                    ticket
                }
                Err(error) => {
                    warn!(%session_id, %error, "could not issue relay ticket");
                    return;
                }
            },
        };
        let (controller, host) = (s.controller.clone(), s.host.clone());
        drop(s); // release the lock before sending

        let msg = ServerMsg::RelayAssign {
            session_id: session_id.clone(),
            relay_addr: self.relay_addr,
            ticket,
        };
        info!(%session_id, "relay assigned");
        self.audit("relay", session_id);
        self.send(&controller, msg.clone());
        self.send(&host, msg);
    }

    /// Explicitly close a session, notifying the peer of the closer.
    pub fn close_session(&self, closer: &DeviceId, session_id: &SessionId) {
        if let Some((_, s)) = self
            .sessions
            .remove_if(session_id, |_, s| s.peer_of(closer).is_some())
        {
            if let Some(peer) = s.peer_of(closer) {
                self.send(
                    peer,
                    ServerMsg::PeerOffline {
                        session_id: session_id.clone(),
                    },
                );
            }
        }
    }

    pub fn record_path(&self, reporter: &DeviceId, session_id: &SessionId, path: TunnelPath) {
        let Some(mut session) = self.sessions.get_mut(session_id) else {
            return;
        };
        if reporter == &session.controller {
            session.controller_path = Some(path);
        } else if reporter == &session.host {
            session.host_path = Some(path);
        }
    }

    // ---- admin snapshots / audit ----

    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        self.devices
            .iter()
            .map(|e| DeviceInfo {
                device_id: e.key().to_string(),
                noise_pubkey: e.value().noise_pubkey.chars().take(16).collect(),
                uptime_secs: e.value().connected_at.elapsed().as_secs(),
                idle_secs: e.value().last_seen.elapsed().as_secs(),
            })
            .collect()
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .iter()
            .map(|e| SessionInfo {
                session_id: e.key().clone(),
                controller: e.value().controller.to_string(),
                host: e.value().host.to_string(),
                relayed: e.value().path() == Some(TunnelPath::Relay),
                path: e.value().path(),
                age_secs: e.value().created_at.elapsed().as_secs(),
            })
            .collect()
    }

    /// Most recent audit entries, newest first.
    pub fn recent_audit(&self, limit: usize) -> Vec<AuditEntry> {
        let log = self.audit.lock().unwrap();
        log.iter().rev().take(limit).cloned().collect()
    }

    fn audit(&self, kind: &str, detail: &str) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut log = self.audit.lock().unwrap();
        if log.len() >= AUDIT_CAPACITY {
            log.pop_front();
        }
        log.push_back(AuditEntry {
            ts_ms,
            kind: kind.to_string(),
            detail: detail.to_string(),
        });
    }
}

/// Random lowercase-hex token of `n` bytes.
fn rand_token(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spuria_common::{candidate::SessionOffer, protocol::ErrorCode};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    fn relay() -> SocketAddr {
        "127.0.0.1:21118".parse().unwrap()
    }

    fn add(r: &Registry, id: &str, pk: &str) -> UnboundedReceiver<ServerMsg> {
        let (tx, rx) = unbounded_channel();
        let (ktx, _krx) = oneshot::channel();
        r.register(DeviceId::new(id), pk.into(), tx, ktx);
        rx
    }

    #[tokio::test]
    async fn full_pairing_and_relay_flow() {
        let r = Registry::new(relay(), "unit-test-relay-secret-32-bytes-long").unwrap();
        let controller = DeviceId::new("ctrl");
        let host = DeviceId::new("host");
        let mut crx = add(&r, "ctrl", "ctrlpk");
        let mut hrx = add(&r, "host", "hostpk");

        r.start_session(&controller, &host);

        let session_id = match crx.recv().await.unwrap() {
            ServerMsg::Punch {
                session_id,
                peer_id,
                peer_noise_pubkey,
                initiator,
            } => {
                assert_eq!(peer_id, host);
                assert_eq!(peer_noise_pubkey, "hostpk");
                assert!(initiator, "controller must be the initiator");
                session_id
            }
            other => panic!("expected punch, got {other:?}"),
        };
        match hrx.recv().await.unwrap() {
            ServerMsg::Punch {
                peer_id,
                peer_noise_pubkey,
                initiator,
                ..
            } => {
                assert_eq!(peer_id, controller);
                assert_eq!(peer_noise_pubkey, "ctrlpk");
                assert!(!initiator);
            }
            other => panic!("expected punch, got {other:?}"),
        }

        let offer = SessionOffer {
            candidates: vec![],
            quic_cert_fp: "fp".into(),
        };
        r.forward_candidates(&controller, &session_id, offer);
        match hrx.recv().await.unwrap() {
            ServerMsg::Candidates { session_id: s, .. } => assert_eq!(s, session_id),
            other => panic!("expected candidates, got {other:?}"),
        }

        r.assign_relay(&controller, &session_id);
        let t_ctrl = match crx.recv().await.unwrap() {
            ServerMsg::RelayAssign {
                ticket, relay_addr, ..
            } => {
                assert_eq!(relay_addr, relay());
                ticket
            }
            other => panic!("expected relay assign, got {other:?}"),
        };
        let t_host = match hrx.recv().await.unwrap() {
            ServerMsg::RelayAssign { ticket, .. } => ticket,
            other => panic!("expected relay assign, got {other:?}"),
        };
        assert_eq!(t_ctrl, t_host, "both peers must share one ticket");

        // Snapshots reflect the live state.
        assert_eq!(r.list_devices().len(), 2);
        assert_eq!(r.list_sessions().len(), 1);
        assert!(!r.list_sessions()[0].relayed);
        assert_eq!(r.list_sessions()[0].path, None);
        r.record_path(&controller, &session_id, TunnelPath::P2p);
        assert_eq!(r.list_sessions()[0].path, None);
        r.record_path(&host, &session_id, TunnelPath::P2p);
        assert_eq!(r.list_sessions()[0].path, Some(TunnelPath::P2p));
        assert!(!r.list_sessions()[0].relayed);

        let host_connection = r.devices.get(&host).unwrap().connection_id;
        r.unregister(&host, host_connection);
        match crx.recv().await.unwrap() {
            ServerMsg::PeerOffline { session_id: s } => assert_eq!(s, session_id),
            other => panic!("expected peer offline, got {other:?}"),
        }
        assert_eq!(r.list_devices().len(), 1);
        assert_eq!(r.list_sessions().len(), 0);
    }

    #[tokio::test]
    async fn connecting_to_offline_peer_errors() {
        let r = Registry::new(relay(), "unit-test-relay-secret-32-bytes-long").unwrap();
        let mut crx = add(&r, "ctrl", "pk");
        r.start_session(&DeviceId::new("ctrl"), &DeviceId::new("ghost"));
        match crx.recv().await.unwrap() {
            ServerMsg::Error { code, .. } => assert_eq!(code, ErrorCode::PeerOffline),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn kick_fires_and_audits() {
        let r = Registry::new(relay(), "unit-test-relay-secret-32-bytes-long").unwrap();
        let (tx, _rx) = unbounded_channel();
        let (ktx, krx) = oneshot::channel();
        r.register(DeviceId::new("victim"), "pk".into(), tx, ktx);
        assert!(r.kick(&DeviceId::new("victim")));
        assert!(krx.await.is_ok(), "kick signal should be delivered");
        // A second kick is a no-op (already taken).
        assert!(!r.kick(&DeviceId::new("victim")));
        assert!(r.recent_audit(10).iter().any(|e| e.kind == "kick"));
    }

    #[tokio::test]
    async fn old_connection_cannot_dispatch_or_unregister_replacement() {
        let r = Registry::new(relay(), "unit-test-relay-secret-32-bytes-long").unwrap();
        let id = DeviceId::new("reconnecting");
        let (old_tx, _old_rx) = unbounded_channel();
        let (old_kick_tx, old_kick_rx) = oneshot::channel();
        let old = r.register(id.clone(), "old".into(), old_tx, old_kick_tx);
        let (new_tx, _new_rx) = unbounded_channel();
        let (new_kick_tx, mut new_kick_rx) = oneshot::channel();
        let new = r.register(id.clone(), "new".into(), new_tx, new_kick_tx);
        assert_ne!(old, new);
        assert!(old_kick_rx.await.is_ok());
        r.with_connection(&id, old, || panic!("stale connection was authorized"));
        r.unregister(&id, old);
        assert_eq!(r.online_count(), 1);
        assert_eq!(r.pubkey_of(&id).as_deref(), Some("new"));
        assert!(matches!(
            new_kick_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        let called = std::cell::Cell::new(false);
        r.with_connection(&id, new, || called.set(true));
        assert!(called.get());
        r.unregister(&id, new);
        assert_eq!(r.online_count(), 0);
    }

    #[tokio::test]
    async fn nonparticipant_cannot_close_session_or_report_path() {
        let r = Registry::new(relay(), "unit-test-relay-secret-32-bytes-long").unwrap();
        let mut crx = add(&r, "ctrl", "ctrlpk");
        let mut hrx = add(&r, "host", "hostpk");
        let ctrl = DeviceId::new("ctrl");
        let host = DeviceId::new("host");
        r.start_session(&ctrl, &host);
        let sid = match crx.recv().await.unwrap() {
            ServerMsg::Punch { session_id, .. } => session_id,
            other => panic!("unexpected message: {other:?}"),
        };
        hrx.recv().await.unwrap();
        r.close_session(&DeviceId::new("outsider"), &sid);
        r.record_path(&DeviceId::new("outsider"), &sid, TunnelPath::Relay);
        assert_eq!(r.session_count(), 1);
        assert_eq!(r.list_sessions()[0].path, None);
        assert!(hrx.try_recv().is_err());
        r.record_path(&ctrl, &sid, TunnelPath::Relay);
        r.record_path(&host, &sid, TunnelPath::P2p);
        assert_eq!(r.list_sessions()[0].path, None);
        r.record_path(&host, &sid, TunnelPath::Relay);
        assert!(r.list_sessions()[0].relayed);
        r.close_session(&ctrl, &sid);
        assert_eq!(r.session_count(), 0);
        assert!(matches!(
            hrx.recv().await,
            Some(ServerMsg::PeerOffline { .. })
        ));
    }
}
