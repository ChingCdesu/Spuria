//! Own native RDP launch work for exactly one client run. Network events, not
//! frontend requests, authorize the destination and lifetime of each launch.
use serde::Serialize;
use spuria_client::ClientEvent;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::task::JoinSet;

type Launcher<T> = Arc<dyn Fn(SocketAddr) -> Result<T, String> + Send + Sync>;

#[derive(Clone, Serialize)]
pub struct RdpLaunchEvent {
    pub session_id: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

struct Session<T> {
    cancelled: Arc<AtomicBool>,
    attempted: bool,
    resource: Option<T>,
}

enum Finished<T> {
    Launch(String, Result<Option<T>, String>),
    Cleanup,
}

pub struct RdpLaunchCoordinator<T: Send + 'static> {
    expected_peer: String,
    configured_address: SocketAddr,
    stopping: Arc<AtomicBool>,
    launcher: Option<Launcher<T>>,
    sessions: HashMap<String, Session<T>>,
    work: JoinSet<Finished<T>>,
}

impl<T: Send + 'static> RdpLaunchCoordinator<T> {
    pub fn new(
        expected_peer: String,
        configured_address: SocketAddr,
        stopping: Arc<AtomicBool>,
        launcher: Option<Launcher<T>>,
    ) -> Self {
        Self {
            expected_peer,
            configured_address,
            stopping,
            launcher,
            sessions: HashMap::new(),
            work: JoinSet::new(),
        }
    }

    pub fn on_event(&mut self, event: &ClientEvent) -> Option<RdpLaunchEvent> {
        if self.launcher.is_none() || self.stopping.load(Ordering::SeqCst) {
            return None;
        }
        match event {
            ClientEvent::SessionStarted {
                session_id,
                peer_id,
            } if peer_id == &self.expected_peer => {
                // Never reset an already ended or launched session on a duplicate event.
                self.sessions
                    .entry(session_id.clone())
                    .or_insert_with(|| Session {
                        cancelled: Arc::new(AtomicBool::new(false)),
                        attempted: false,
                        resource: None,
                    });
            }
            ClientEvent::RdpReady {
                session_id,
                listen_addr,
            } => {
                let session = self.sessions.get_mut(session_id)?;
                if session.attempted || session.cancelled.load(Ordering::SeqCst) {
                    return None;
                }
                session.attempted = true;
                let address = listen_addr.parse::<SocketAddr>().ok().filter(|address| {
                    address.ip().is_loopback()
                        && address.port() != 0
                        && address.ip() == self.configured_address.ip()
                        && (self.configured_address.port() == 0
                            || address.port() == self.configured_address.port())
                });
                let Some(address) = address else {
                    return Some(failure(
                        session_id,
                        "Remote Desktop requires the current tunnel's loopback listener.",
                    ));
                };
                let cancelled = session.cancelled.clone();
                let stopping = self.stopping.clone();
                let launcher = self.launcher.as_ref().unwrap().clone();
                let id = session_id.clone();
                self.work.spawn_blocking(move || {
                    if stopping.load(Ordering::SeqCst) || cancelled.load(Ordering::SeqCst) {
                        return Finished::Launch(id, Ok(None));
                    }
                    let result = launcher(address);
                    // A blocking Windows API cannot be aborted safely. If stop won
                    // during it, clean up here, and shutdown waits for this worker.
                    if stopping.load(Ordering::SeqCst) || cancelled.load(Ordering::SeqCst) {
                        drop(result);
                        Finished::Launch(id, Ok(None))
                    } else {
                        Finished::Launch(id, result.map(Some))
                    }
                });
            }
            ClientEvent::SessionEnded { session_id, .. } => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.cancelled.store(true, Ordering::SeqCst);
                    if let Some(resource) = session.resource.take() {
                        self.cleanup(resource);
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn cleanup(&mut self, resource: T) {
        self.work.spawn_blocking(move || {
            drop(resource);
            Finished::Cleanup
        });
    }

    /// Polled alongside the network future. No pending worker means wait until
    /// the caller selects another event, drops this future, and polls again.
    pub async fn next_event(&mut self) -> RdpLaunchEvent {
        loop {
            let Some(completed) = self.work.join_next().await else {
                return std::future::pending().await;
            };
            match completed {
                Ok(Finished::Launch(id, result)) => {
                    let active = !self.stopping.load(Ordering::SeqCst)
                        && self
                            .sessions
                            .get(&id)
                            .is_some_and(|s| !s.cancelled.load(Ordering::SeqCst));
                    match result {
                        Ok(Some(resource)) if active => {
                            self.sessions.get_mut(&id).unwrap().resource = Some(resource);
                            return RdpLaunchEvent {
                                session_id: id,
                                status: "launched",
                                message: None,
                            };
                        }
                        Ok(Some(resource)) => self.cleanup(resource),
                        Err(message) if active => return failure(&id, &message),
                        _ => {}
                    }
                }
                // A failed worker must never make the tunnel itself fail. The
                // listener address remains available for manual connection.
                Err(_) => {
                    if let Some((id, _)) = self.sessions.iter().find(|(_, s)| {
                        s.attempted && s.resource.is_none() && !s.cancelled.load(Ordering::SeqCst)
                    }) {
                        return failure(id, "Could not start Windows Remote Desktop. Connect manually using the tunnel address.");
                    }
                }
                Ok(Finished::Cleanup) => {}
            }
        }
    }

    pub async fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        for session in self.sessions.values_mut() {
            session.cancelled.store(true, Ordering::SeqCst);
        }
        let resources: Vec<T> = self
            .sessions
            .values_mut()
            .filter_map(|s| s.resource.take())
            .collect();
        for resource in resources {
            self.cleanup(resource);
        }
        // spawn_blocking cannot be cancelled by aborting its JoinHandle. Drain
        // it, including resources returned after cancellation, before exit.
        while let Some(completed) = self.work.join_next().await {
            if let Ok(Finished::Launch(_, Ok(Some(resource)))) = completed {
                self.cleanup(resource);
            }
        }
        self.sessions.clear();
        self.launcher = None;
    }
}

impl<T: Send + 'static> Drop for RdpLaunchCoordinator<T> {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        for session in self.sessions.values() {
            session.cancelled.store(true, Ordering::SeqCst);
        }
        // Normal shutdown drains workers. This also prevents a pending worker
        // from keeping a newly launched process if its owner unwinds.
    }
}

fn failure(session_id: &str, message: &str) -> RdpLaunchEvent {
    RdpLaunchEvent {
        session_id: session_id.into(),
        status: "failed",
        message: Some(message.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, mpsc};
    use std::time::Duration;

    struct Resource(Arc<AtomicUsize>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn started(id: &str, peer: &str) -> ClientEvent {
        ClientEvent::SessionStarted {
            session_id: id.into(),
            peer_id: peer.into(),
        }
    }
    fn ready(id: &str, address: &str) -> ClientEvent {
        ClientEvent::RdpReady {
            session_id: id.into(),
            listen_addr: address.into(),
        }
    }
    fn ended(id: &str) -> ClientEvent {
        ClientEvent::SessionEnded {
            session_id: id.into(),
            error: None,
        }
    }
    fn runtime(
        port: u16,
    ) -> (
        RdpLaunchCoordinator<Resource>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let launches = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let l = launches.clone();
        let d = drops.clone();
        let coordinator = RdpLaunchCoordinator::new(
            "expected".into(),
            ([127, 0, 0, 1], port).into(),
            Arc::new(AtomicBool::new(false)),
            Some(Arc::new(move |_| {
                l.fetch_add(1, Ordering::SeqCst);
                Ok(Resource(d.clone()))
            })),
        );
        (coordinator, launches, drops)
    }

    #[tokio::test]
    async fn only_matching_ready_launches_once_and_ended_cleans_up() {
        let (mut c, launches, drops) = runtime(0);
        c.on_event(&ready("unknown", "127.0.0.1:54321"));
        c.on_event(&started("wrong", "another-peer"));
        c.on_event(&ready("wrong", "127.0.0.1:54321"));
        c.on_event(&started("right", "expected"));
        c.on_event(&ClientEvent::TunnelUp {
            session_id: "right".into(),
            path: "quic".into(),
        });
        assert!(c.work.is_empty());
        c.on_event(&ready("right", "127.0.0.1:54321"));
        c.on_event(&ready("right", "127.0.0.1:54321"));
        assert_eq!(c.next_event().await.status, "launched");
        c.on_event(&ended("right"));
        c.on_event(&started("right", "expected"));
        c.on_event(&ready("right", "127.0.0.1:54321"));
        c.shutdown().await;
        assert_eq!(launches.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mismatched_or_non_loopback_addresses_are_rejected() {
        for addr in [
            "127.0.0.1:0",
            "127.0.0.1:33390",
            "127.0.0.2:33389",
            "0.0.0.0:33389",
            "192.0.2.1:33389",
            "bad",
        ] {
            let (mut c, launches, _) = runtime(33389);
            c.on_event(&started("s", "expected"));
            assert_eq!(c.on_event(&ready("s", addr)).unwrap().status, "failed");
            c.shutdown().await;
            assert_eq!(launches.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn stop_before_queued_ready_prevents_launch() {
        let (mut c, launches, _) = runtime(0);
        c.on_event(&started("s", "expected"));
        c.stopping.store(true, Ordering::SeqCst);
        c.on_event(&ready("s", "127.0.0.1:54321"));
        c.shutdown().await;
        assert_eq!(launches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shutdown_waits_for_inflight_worker_and_cleans_late_resource() {
        let drops = Arc::new(AtomicUsize::new(0));
        let d = drops.clone();
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let entered = std::sync::Mutex::new(Some(entered));
        let (release, release_rx) = mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let stopping = Arc::new(AtomicBool::new(false));
        let mut c = RdpLaunchCoordinator::new(
            "expected".into(),
            ([127, 0, 0, 1], 0).into(),
            stopping.clone(),
            Some(Arc::new(move |_| {
                let _ = entered.lock().unwrap().take().unwrap().send(());
                release_rx.lock().unwrap().recv().unwrap();
                Ok(Resource(d.clone()))
            })),
        );
        c.on_event(&started("s", "expected"));
        c.on_event(&ready("s", "127.0.0.1:54321"));
        entered_rx.await.unwrap();
        let shutdown = tokio::spawn(async move { c.shutdown().await });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(stopping.load(Ordering::SeqCst));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn launch_failure_preserves_manual_fallback_without_retry() {
        let mut c = RdpLaunchCoordinator::<Resource>::new(
            "expected".into(),
            ([127, 0, 0, 1], 0).into(),
            Arc::new(AtomicBool::new(false)),
            Some(Arc::new(|_| {
                Err("Remote Desktop executable is unavailable.".into())
            })),
        );
        c.on_event(&started("s", "expected"));
        c.on_event(&ready("s", "127.0.0.1:54321"));
        let event = c.next_event().await;
        assert_eq!(event.status, "failed");
        c.on_event(&ready("s", "127.0.0.1:54321"));
        assert!(c.work.is_empty());
        c.shutdown().await;
    }
}
