//! In-process end-to-end regression test.
//!
//! Boots the signaling + relay servers and a fake RDP echo service, runs a
//! host and a controller session, then pushes bytes through the controller's
//! local listener and verifies the echo — for both the QUIC P2P path and the
//! Noise relay path. Cross-platform (runs on Linux CI).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use spuria_client::{app, AppConfig, ClientEvent};
use spuria_common::{auth::AllowAllAuth, ids::DeviceId, transport::Role};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

fn free_tcp() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_udp() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

async fn echo_server(addr: SocketAddr) {
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (mut sock, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

async fn udp_echo_server(addr: SocketAddr) {
    let sock = UdpSocket::bind(addr).await.unwrap();
    let mut buf = vec![0u8; 65535];
    loop {
        if let Ok((n, src)) = sock.recv_from(&mut buf).await {
            let _ = sock.send_to(&buf[..n], src).await;
        }
    }
}

/// Wait (bounded) for an event matching `pred`, returning it.
async fn wait_ev<F>(rx: &mut UnboundedReceiver<ClientEvent>, pred: F) -> Option<ClientEvent>
where
    F: Fn(&ClientEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) if pred(&ev) => return Some(ev),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

async fn run_path(force_relay: bool) {
    let ws = free_tcp();
    let reflect = free_udp();
    let relay = free_tcp();
    let echo = free_tcp();
    let listen = free_tcp();

    tokio::spawn(spuria_signaling::run(spuria_signaling::SignalingConfig {
        ws_bind: local(ws),
        reflect_bind: local(reflect),
        relay_addr: local(relay),
        relay_secret: "spuria-e2e-only-ticket-secret-32-bytes".into(),
        auth: Arc::new(AllowAllAuth),
        admin_bind: None,
        admin_token: None,
        max_conn_per_sec: 1000.0,
    }));
    tokio::spawn(spuria_relay::run(spuria_relay::RelayConfig {
        bind: local(relay),
        relay_secret: "spuria-e2e-only-ticket-secret-32-bytes".into(),
        park_timeout: Duration::from_secs(30),
        hello_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(300),
        max_connections: 128,
        max_ticket_records: 256,
        max_sessions: 64,
        max_conn_per_sec: 1000.0,
    }));
    tokio::spawn(echo_server(local(echo)));
    tokio::spawn(udp_echo_server(local(echo)));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let server_url = format!("ws://127.0.0.1:{ws}");
    let reflect_addr = local(reflect);

    // Host (被控).
    let (host_tx, mut host_rx) = unbounded_channel();
    let host_dir = std::env::temp_dir().join(format!("spuria-it-host-{ws}"));
    let _ = std::fs::remove_dir_all(&host_dir);
    tokio::spawn(app::run(AppConfig {
        role: Role::Host,
        server_url: server_url.clone(),
        reflect_addr,
        device_id: DeviceId::new("100000001"),
        secret: String::new(),
        peer_id: None,
        rdp_addr: local(echo),
        listen_addr: local(0),
        data_dir: host_dir,
        force_relay,
        enable_udp: true,
        events: Some(host_tx),
        allow_forward_ports: vec![],
        initial_forwards: vec![],
        controls: None,
    }));
    assert!(
        wait_ev(&mut host_rx, |e| matches!(
            e,
            ClientEvent::Registered { .. }
        ))
        .await
        .is_some(),
        "host failed to register"
    );

    // Controller (主控).
    let (ctrl_tx, mut ctrl_rx) = unbounded_channel();
    let ctrl_dir = std::env::temp_dir().join(format!("spuria-it-ctrl-{ws}"));
    let _ = std::fs::remove_dir_all(&ctrl_dir);
    tokio::spawn(app::run(AppConfig {
        role: Role::Controller,
        server_url,
        reflect_addr,
        device_id: DeviceId::new("100000002"),
        secret: String::new(),
        peer_id: Some(DeviceId::new("100000001")),
        rdp_addr: local(0),
        listen_addr: local(listen),
        data_dir: ctrl_dir,
        force_relay,
        enable_udp: true,
        events: Some(ctrl_tx),
        allow_forward_ports: vec![],
        initial_forwards: vec![],
        controls: None,
    }));

    // Assert the expected path was taken.
    let tunnel = wait_ev(&mut ctrl_rx, |e| matches!(e, ClientEvent::TunnelUp { .. }))
        .await
        .expect("tunnel never came up");
    if let ClientEvent::TunnelUp { path, .. } = tunnel {
        let expected = if force_relay { "relay" } else { "p2p" };
        assert_eq!(path, expected, "unexpected tunnel path");
    }

    // Wait for the controller's local listener.
    assert!(
        wait_ev(&mut ctrl_rx, |e| matches!(e, ClientEvent::RdpReady { .. }))
            .await
            .is_some(),
        "controller RDP listener not ready"
    );

    // TCP probe: connect, send, verify the echo round-tripped through the tunnel.
    // Keep `sock` open for the rest of the test so the session (and the UDP
    // forwarder) stays alive.
    let mut sock = TcpStream::connect(local(listen)).await.unwrap();
    let msg = if force_relay {
        b"relay-path-bytes".to_vec()
    } else {
        b"p2p-path-bytes".to_vec()
    };
    sock.write_all(&msg).await.unwrap();
    let mut buf = vec![0u8; msg.len()];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut buf))
        .await
        .expect("echo read timed out")
        .expect("echo read failed");
    assert_eq!(buf, msg, "echoed bytes did not match");

    // UDP multitransport probe (P2P/datagram path only). Retried because UDP is
    // lossy and the controller's UDP listener binds lazily after TCP connects.
    if !force_relay {
        let uc = UdpSocket::bind(local(0)).await.unwrap();
        uc.connect(local(listen)).await.unwrap();
        let umsg = b"udp-multitransport-probe";
        let mut ok = false;
        for _ in 0..25 {
            let _ = uc.send(umsg).await;
            let mut ubuf = vec![0u8; umsg.len()];
            match tokio::time::timeout(Duration::from_millis(300), uc.recv(&mut ubuf)).await {
                Ok(Ok(n)) if &ubuf[..n] == umsg => {
                    ok = true;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        assert!(
            ok,
            "UDP multitransport echo did not round-trip over QUIC datagrams"
        );
    }

    drop(sock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_p2p_path() {
    tokio::time::timeout(Duration::from_secs(45), run_path(false))
        .await
        .expect("p2p e2e timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_relay_path() {
    tokio::time::timeout(Duration::from_secs(45), run_path(true))
        .await
        .expect("relay e2e timed out");
}
