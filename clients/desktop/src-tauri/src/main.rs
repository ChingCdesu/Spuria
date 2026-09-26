// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Spuria desktop client (Tauri 2).
//!
//! The GUI drives the same [`spuria_client::app::run`] state machine as the CLI.
//! A `connect` command spawns the session on Tauri's async runtime and forwards
//! [`spuria_client::ClientEvent`]s to the frontend as `client-event` events; a
//! `disconnect` command stops and awaits it. The system RDP bridge / IronRDP integration
//! is unchanged from the library — the GUI is purely a front-end over it.

use std::{net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};
use spuria_client::{app, AppConfig};
use spuria_common::{ids::DeviceId, transport::Role};
use tauri::{async_runtime::JoinHandle, AppHandle, Emitter, Manager, State};
use tokio::sync::{oneshot, Mutex};

/// Holds the currently-running session task (if any).
#[derive(Default)]
struct AppState {
    task: Mutex<Option<RunningClient>>,
}

struct RunningClient {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl RunningClient {
    async fn shutdown(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

/// Global, persisted client configuration — edited on the Settings page and
/// stored in `settings.json` under the app data dir. `#[serde(default)]` keeps
/// old files loadable as new fields are added.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    // Network
    server: String,
    reflect: String,
    secret: String,
    // Connection defaults
    default_listen: String,
    default_rdp: String,
    force_relay: bool,
    enable_udp: bool,
    // Appearance: "system" | "light" | "dark"
    theme: String,
    // Updates
    auto_check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: "ws://127.0.0.1:21116".into(),
            reflect: "127.0.0.1:21117".into(),
            secret: String::new(),
            default_listen: "127.0.0.1:33389".into(),
            default_rdp: "127.0.0.1:3389".into(),
            force_relay: false,
            enable_udp: true,
            theme: "system".into(),
            auto_check_updates: true,
        }
    }
}

/// App metadata for the Settings → About section.
#[derive(Serialize)]
struct AppInfo {
    version: String,
    device_id: String,
}

/// Per-connection options sent from the frontend's Connect form. Global
/// settings (server / reflect / secret) come from [`Settings`], not here.
#[derive(Deserialize)]
struct ConnectOpts {
    /// "controller" or "host".
    role: String,
    device_id: Option<String>,
    /// Controller only: the host device id to reach.
    peer_id: Option<String>,
    /// Controller only: local listener for the RDP client.
    listen: Option<String>,
    /// Host only: local RDP service address.
    rdp: Option<String>,
    #[serde(default)]
    force_relay: bool,
}

fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Result<Settings, String> {
    let path = settings_path(app)?;
    if !path.exists() {
        return Ok(Settings::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Read the persisted global settings (defaults if none saved yet).
#[tauri::command]
fn get_settings(app: AppHandle) -> Result<Settings, String> {
    load_settings(&app)
}

/// Persist the global settings to local disk.
#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let text = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(settings_path(&app)?, text).map_err(|e| e.to_string())
}

/// Resolve a stable device id: explicit override, else persisted, else fresh.
fn resolve_device_id(app: &AppHandle, override_id: Option<String>) -> Result<String, String> {
    if let Some(id) = override_id.filter(|s| !s.trim().is_empty()) {
        return Ok(id.trim().to_string());
    }
    let path = data_dir(app)?.join("device_id.txt");
    if path.exists() {
        return Ok(std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())?
            .trim()
            .to_string());
    }
    let id = DeviceId::random().to_string();
    std::fs::write(&path, &id).map_err(|e| e.to_string())?;
    Ok(id)
}

/// Return (and lazily create) this device's id for display in the UI.
#[tauri::command]
fn ensure_device_id(app: AppHandle) -> Result<String, String> {
    resolve_device_id(&app, None)
}

/// App version + this device's id, for the Settings → About section.
#[tauri::command]
fn get_app_info(app: AppHandle) -> Result<AppInfo, String> {
    Ok(AppInfo {
        version: app.package_info().version.to_string(),
        device_id: resolve_device_id(&app, None)?,
    })
}

#[tauri::command]
async fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    opts: ConnectOpts,
) -> Result<String, String> {
    let settings = load_settings(&app)?;
    if settings.server.trim().is_empty() {
        return Err("No signaling server configured — open Settings and save first.".into());
    }
    let role = match opts.role.as_str() {
        "host" => Role::Host,
        _ => Role::Controller,
    };
    let reflect_addr: SocketAddr = settings
        .reflect
        .trim()
        .parse()
        .map_err(|e| format!("bad reflect address in settings: {e}"))?;
    let listen_str = opts
        .listen
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| settings.default_listen.clone());
    let listen_addr: SocketAddr = listen_str
        .trim()
        .parse()
        .map_err(|e| format!("bad listen address: {e}"))?;
    let rdp_str = opts
        .rdp
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| settings.default_rdp.clone());
    let rdp_addr: SocketAddr = rdp_str
        .trim()
        .parse()
        .map_err(|e| format!("bad rdp address: {e}"))?;
    let device_id = resolve_device_id(&app, opts.device_id)?;
    let peer_id = opts
        .peer_id
        .filter(|s| !s.trim().is_empty())
        .map(|s| DeviceId::new(s.trim().to_string()));

    if role == Role::Controller && peer_id.is_none() {
        return Err("controller mode requires a peer device id".into());
    }

    // Event forwarding is owned by the same task as the client, so an old
    // connection cannot emit stale events after its replacement starts.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let cfg = AppConfig {
        role,
        server_url: settings.server,
        reflect_addr,
        device_id: DeviceId::new(device_id.clone()),
        secret: settings.secret,
        peer_id,
        rdp_addr,
        listen_addr,
        data_dir: data_dir(&app)?,
        force_relay: opts.force_relay,
        enable_udp: settings.enable_udp,
        events: Some(tx),
    };

    // Serialize replacement and disconnect through the whole shutdown. The
    // command returns only after previous RDP listeners/bridges have stopped.
    let mut running = state.task.lock().await;
    if let Some(previous) = running.take() {
        previous.shutdown().await;
    }
    let (stop, stopped) = oneshot::channel();
    let app_for_run = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let run = app::run_until_shutdown(cfg, async {
            let _ = stopped.await;
        });
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                Some(ev) = rx.recv() => { let _ = app_for_run.emit("client-event", ev); }
            }
        };
        while let Ok(ev) = rx.try_recv() {
            let _ = app_for_run.emit("client-event", ev);
        }
        if let Err(e) = result {
            let _ = app_for_run.emit("client-error", e.to_string());
        }
        let _ = app_for_run.emit("client-stopped", ());
    });
    *running = Some(RunningClient { stop, task: handle });

    Ok(device_id)
}

#[tauri::command]
async fn disconnect(state: State<'_, AppState>) -> Result<(), String> {
    let mut running = state.task.lock().await;
    if let Some(client) = running.take() {
        client.shutdown().await;
    }
    Ok(())
}

/// Check the configured update endpoint and install if a newer version exists.
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<String, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(update) => {
            let version = update.version.clone();
            update
                .download_and_install(|_downloaded, _total| {}, || {})
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!("Updated to {version}. Restart to apply."))
        }
        None => Ok("You are on the latest version.".into()),
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            ensure_device_id,
            get_app_info,
            get_settings,
            save_settings,
            connect,
            disconnect,
            check_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running Spuria desktop");
}
