//! `spuria-relay` — the relay (data-plane) server binary.

use anyhow::Result;
use clap::Parser;
use spuria_common::defaults;
use spuria_relay::server::{self, RelayConfig};
use std::{net::SocketAddr, time::Duration};

#[derive(Parser, Debug)]
#[command(
    name = "spuria-relay",
    about = "Spuria relay server (blind ciphertext forwarder)"
)]
struct Cli {
    /// Address to bind the relay TCP listener on.
    #[arg(long, env = "SPURIA_RELAY_BIND", default_value_t = default_bind())]
    bind: SocketAddr,

    /// Seconds a first arrival waits to be paired before eviction.
    #[arg(long, default_value_t = 30)]
    park_timeout_secs: u64,

    /// Maximum concurrent spliced sessions.
    #[arg(long, default_value_t = 256)]
    max_sessions: u64,

    /// Max sustained new connections per second per source IP (burst = 3x).
    #[arg(long, default_value_t = 20.0)]
    max_conn_per_sec: f64,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], defaults::RELAY_PORT))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    server::run(RelayConfig {
        bind: cli.bind,
        park_timeout: Duration::from_secs(cli.park_timeout_secs),
        max_sessions: cli.max_sessions,
        max_conn_per_sec: cli.max_conn_per_sec,
    })
    .await
}
