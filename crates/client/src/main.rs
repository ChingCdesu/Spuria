//! `spuria` — the tunnel client binary (controller + host roles).
//!
//! Primarily a headless/automation entrypoint and the test harness driver;
//! the user-facing client is the Tauri GUI in `clients/desktop`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use spuria_client::app;
use spuria_client::forwarding::ForwardRule;
use spuria_common::{defaults, ids::DeviceId, transport::Role};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "spuria", about = "Spuria P2P RDP tunnel client")]
struct Cli {
    /// Signaling server URL (ws://host:port).
    #[arg(long, env = "SPURIA_SERVER", default_value = "ws://127.0.0.1:21116")]
    server: String,

    /// Reflection (srflx) service address (UDP host:port).
    #[arg(long, env = "SPURIA_REFLECT", default_value = "127.0.0.1:21117")]
    reflect: SocketAddr,

    /// This device's id. If omitted, a stable one is generated under --data-dir.
    #[arg(long, env = "SPURIA_DEVICE_ID")]
    device_id: Option<String>,

    /// Shared team secret (must match the signaling server's).
    #[arg(long, env = "SPURIA_SECRET", default_value = "")]
    secret: String,

    /// Directory for the persistent device key and id.
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,

    /// Skip QUIC P2P and use the relay directly (testing / constrained nets).
    #[arg(long, env = "SPURIA_FORCE_RELAY")]
    force_relay: bool,

    /// Disable RDP UDP multitransport (no QUIC datagram forwarding).
    #[arg(long, env = "SPURIA_NO_UDP")]
    no_udp: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run as the controlled host (被控): bridge the tunnel to local RDP.
    Host {
        /// Local RDP service address.
        #[arg(long, default_value_t = SocketAddr::from(([127, 0, 0, 1], defaults::RDP_PORT)))]
        rdp: SocketAddr,
        /// Explicitly allow forwarding to these host loopback TCP ports.
        /// Repeat the flag or separate ports with commas; defaults to none.
        #[arg(long = "allow-port", value_delimiter = ',')]
        allow_ports: Vec<u16>,
    },
    /// Run as the controller (主控): connect to a peer and expose a local
    /// listener for your RDP client.
    Control {
        /// Device id of the host to connect to.
        peer: String,
        /// Local address your RDP client should connect to.
        #[arg(long, default_value_t = SocketAddr::from(([127, 0, 0, 1], 33389)))]
        listen: SocketAddr,
        /// Start a TCP forward: LOOPBACK_LISTEN_ADDR:PORT=REMOTE_PORT.
        /// May be repeated; the host must explicitly allow each remote port.
        #[arg(long = "forward", value_parser = parse_forward)]
        forwards: Vec<ForwardSpec>,
    },
}

#[derive(Clone, Debug)]
struct ForwardSpec {
    listen_addr: SocketAddr,
    remote_port: u16,
}

fn parse_forward(value: &str) -> std::result::Result<ForwardSpec, String> {
    let (listen, remote) = value.rsplit_once('=').ok_or_else(||
        "forward must be LOOPBACK_LISTEN_ADDR:PORT=REMOTE_PORT (for example 127.0.0.1:18080=8080)".to_string())?;
    let listen_addr = listen
        .parse::<SocketAddr>()
        .map_err(|_| "invalid forwarding listen address".to_string())?;
    let remote_port = remote
        .parse::<u16>()
        .map_err(|_| "invalid remote forwarding port".to_string())?;
    app::validate_forward_rule(&ForwardRule {
        id: "cli".to_string(),
        listen_addr,
        remote_port,
    })?;
    Ok(ForwardSpec {
        listen_addr,
        remote_port,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let (role, peer_id, rdp_addr, listen_addr, allow_forward_ports, initial_forwards) =
        match &cli.cmd {
            Cmd::Host { rdp, allow_ports } => (
                Role::Host,
                None,
                *rdp,
                SocketAddr::from(([127, 0, 0, 1], 0)),
                allow_ports.clone(),
                vec![],
            ),
            Cmd::Control {
                peer,
                listen,
                forwards,
            } => (
                Role::Controller,
                Some(DeviceId::new(peer.clone())),
                SocketAddr::from(([127, 0, 0, 1], defaults::RDP_PORT)),
                *listen,
                vec![],
                forwards
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| ForwardRule {
                        id: format!("cli-{}", index + 1),
                        listen_addr: rule.listen_addr,
                        remote_port: rule.remote_port,
                    })
                    .collect(),
            ),
        };
    app::validate_forward_ports(&allow_forward_ports).map_err(anyhow::Error::msg)?;
    if initial_forwards.len() > 32 {
        anyhow::bail!("at most 32 forwarding rules may be configured");
    }
    let device_id = resolve_device_id(cli.device_id.clone(), &cli.data_dir)?;

    info!(device_id = %device_id, role = ?role, "spuria device id");

    app::run(app::AppConfig {
        role,
        server_url: cli.server,
        reflect_addr: cli.reflect,
        device_id,
        secret: cli.secret,
        peer_id,
        rdp_addr,
        listen_addr,
        data_dir: cli.data_dir,
        force_relay: cli.force_relay,
        enable_udp: !cli.no_udp,
        events: None,
        allow_forward_ports,
        initial_forwards,
        controls: None,
    })
    .await
}

/// Resolve the device id: explicit flag > persisted file > freshly generated.
fn resolve_device_id(cli_id: Option<String>, data_dir: &Path) -> Result<DeviceId> {
    if let Some(id) = cli_id {
        return Ok(DeviceId::new(id));
    }
    let path = data_dir.join("device_id.txt");
    if path.exists() {
        let id = std::fs::read_to_string(&path).context("reading device id")?;
        return Ok(DeviceId::new(id.trim().to_string()));
    }
    let id = DeviceId::random();
    std::fs::create_dir_all(data_dir).context("creating data dir")?;
    std::fs::write(&path, id.as_str()).context("persisting device id")?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_parser_accepts_only_loopback_tcp_targets() {
        let v4 = parse_forward("127.0.0.1:18080=8080").unwrap();
        assert_eq!(v4.listen_addr.port(), 18080);
        assert_eq!(v4.remote_port, 8080);
        assert!(parse_forward("[::1]:18080=8080").is_ok());
        assert!(parse_forward("127.0.0.1:0=22").is_ok());
        for invalid in [
            "8080",
            "127.0.0.1:18080=0",
            "0.0.0.0:18080=8080",
            "127.0.0.1:1=65536",
        ] {
            assert!(parse_forward(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn host_allow_ports_accept_repeated_and_comma_delimited_flags() {
        let cli = Cli::try_parse_from([
            "spuria",
            "host",
            "--allow-port",
            "22,8080",
            "--allow-port",
            "8443",
        ])
        .unwrap();
        let Cmd::Host { allow_ports, .. } = cli.cmd else {
            panic!("expected host");
        };
        assert_eq!(allow_ports, vec![22, 8080, 8443]);
    }
}
