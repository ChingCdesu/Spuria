//! Admin HTTP API + embedded web dashboard (monitoring + management + audit).
//!
//! Endpoints (all `/api/*` require `Authorization: Bearer <admin-token>`):
//! * `GET  /api/stats`    — online / session counts
//! * `GET  /api/devices`  — online device table
//! * `GET  /api/sessions` — active session table
//! * `GET  /api/audit`    — recent audit entries (`?limit=`)
//! * `POST /api/kick`     — force-disconnect a device (`{"device_id": "..."}`)
//!
//! `GET /` serves the dashboard SPA, which stores the token in localStorage.

use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use spuria_common::ids::DeviceId;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tracing::info;

use crate::registry::{AuditEntry, DeviceInfo, Registry, SessionInfo};

#[derive(Clone)]
struct AdminState {
    registry: Arc<Registry>,
    token: Arc<String>,
}

/// Start the admin HTTP server. Blocks until the listener errors.
pub async fn serve(registry: Arc<Registry>, bind: SocketAddr, token: String) -> Result<()> {
    let state = AdminState {
        registry,
        token: Arc::new(token),
    };
    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/stats", get(stats))
        .route("/api/devices", get(devices))
        .route("/api/sessions", get(sessions))
        .route("/api/audit", get(audit))
        .route("/api/kick", post(kick))
        .route("/metrics", get(metrics))
        .with_state(state);

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding admin HTTP on {bind}"))?;
    info!(addr = %bind, "admin API + dashboard listening");
    axum::serve(listener, app).await.context("admin server")?;
    Ok(())
}

async fn dashboard() -> Html<&'static str> {
    Html(include_str!("admin_dashboard.html"))
}

/// Bearer-token check. Empty configured token is treated as locked.
fn authorize(state: &AdminState, headers: &HeaderMap) -> Result<(), StatusCode> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected = state.token.as_bytes();
    if !expected.is_empty() && constant_time_eq(provided.as_bytes(), expected) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn stats(State(s): State<AdminState>, headers: HeaderMap) -> Result<Json<Value>, StatusCode> {
    authorize(&s, &headers)?;
    Ok(Json(json!({
        "online": s.registry.online_count(),
        "sessions": s.registry.session_count(),
    })))
}

async fn devices(
    State(s): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DeviceInfo>>, StatusCode> {
    authorize(&s, &headers)?;
    Ok(Json(s.registry.list_devices()))
}

async fn sessions(
    State(s): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SessionInfo>>, StatusCode> {
    authorize(&s, &headers)?;
    Ok(Json(s.registry.list_sessions()))
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<usize>,
}

async fn audit(
    State(s): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditEntry>>, StatusCode> {
    authorize(&s, &headers)?;
    let limit = q.limit.unwrap_or(200).min(2000);
    Ok(Json(s.registry.recent_audit(limit)))
}

#[derive(Deserialize)]
struct KickReq {
    device_id: String,
}

async fn kick(
    State(s): State<AdminState>,
    headers: HeaderMap,
    Json(req): Json<KickReq>,
) -> Result<Json<Value>, StatusCode> {
    authorize(&s, &headers)?;
    let kicked = s.registry.kick(&DeviceId::new(req.device_id));
    Ok(Json(json!({ "ok": kicked })))
}

/// Prometheus text-format metrics (token-gated like the rest of the API).
async fn metrics(State(s): State<AdminState>, headers: HeaderMap) -> Result<String, StatusCode> {
    authorize(&s, &headers)?;
    let online = s.registry.online_count();
    let sessions = s.registry.session_count();
    Ok(format!(
        "# HELP spuria_online_devices Currently registered devices.\n\
         # TYPE spuria_online_devices gauge\n\
         spuria_online_devices {online}\n\
         # HELP spuria_active_sessions Currently active sessions.\n\
         # TYPE spuria_active_sessions gauge\n\
         spuria_active_sessions {sessions}\n"
    ))
}
