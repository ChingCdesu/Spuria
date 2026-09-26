//! `spuria-signaling` — the signaling (control-plane) server binary.

use anyhow::Result;
use clap::Parser;
use spuria_common::{
    auth::{AllowAllAuth, Authenticator, SharedSecretAuth, TokenFileAuth},
    defaults,
};
use spuria_signaling::server::{self, SignalingConfig};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    name = "spuria-signaling",
    about = "Spuria signaling server (registry, srflx, pairing)"
)]
struct Cli {
    /// WebSocket control-plane bind address.
    #[arg(long, env = "SPURIA_WS_BIND", default_value_t = SocketAddr::from(([0,0,0,0], defaults::SIGNALING_PORT)))]
    ws_bind: SocketAddr,

    /// UDP reflection (srflx) bind address.
    #[arg(long, env = "SPURIA_REFLECT_BIND", default_value_t = SocketAddr::from(([0,0,0,0], defaults::REFLECT_PORT)))]
    reflect_bind: SocketAddr,

    /// Public address clients should use to reach the relay server.
    #[arg(long, env = "SPURIA_RELAY_ADDR", default_value_t = SocketAddr::from(([127,0,0,1], defaults::RELAY_PORT)))]
    relay_addr: SocketAddr,

    /// Dedicated signing key shared only with relay, at least 32 random bytes.
    #[arg(long, env = "SPURIA_RELAY_SECRET", hide_env_values = true)]
    relay_secret: String,

    /// Shared team secret. If omitted, authentication is DISABLED (dev only).
    #[arg(long, env = "SPURIA_SECRET")]
    secret: Option<String>,

    /// Admin API + dashboard bind address. If omitted, the admin API is off.
    #[arg(long, env = "SPURIA_ADMIN_BIND")]
    admin_bind: Option<SocketAddr>,

    /// Admin bearer token. Required for the admin API to start.
    #[arg(long, env = "SPURIA_ADMIN_TOKEN")]
    admin_token: Option<String>,

    /// Per-device credentials file (`device_id:token` per line). Takes
    /// precedence over --secret; demonstrates the pluggable Authenticator seam.
    #[arg(long, env = "SPURIA_AUTH_FILE")]
    auth_file: Option<PathBuf>,

    /// Max sustained new connections per second per source IP (burst = 3x).
    #[arg(long, default_value_t = 20.0)]
    max_conn_per_sec: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let auth: Arc<dyn Authenticator> = if let Some(path) = cli.auth_file {
        let auth = TokenFileAuth::load(&path)?;
        info!(devices = auth.len(), ?path, "loaded per-device credentials");
        Arc::new(auth)
    } else if let Some(s) = cli.secret {
        Arc::new(SharedSecretAuth::new(s))
    } else {
        warn!("no --secret or --auth-file set: authentication is DISABLED (development mode)");
        Arc::new(AllowAllAuth)
    };

    server::run(SignalingConfig {
        ws_bind: cli.ws_bind,
        reflect_bind: cli.reflect_bind,
        relay_addr: cli.relay_addr,
        relay_secret: cli.relay_secret,
        auth,
        admin_bind: cli.admin_bind,
        admin_token: cli.admin_token,
        max_conn_per_sec: cli.max_conn_per_sec,
    })
    .await
}
