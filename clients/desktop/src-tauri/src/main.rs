// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Spuria desktop client (Tauri 2).
//!
//! The GUI drives the same client state machine as the CLI.
//! A `connect` command spawns the session on Tauri's async runtime and forwards
//! [`spuria_client::ClientEvent`]s to the frontend as `client-event` events; a
//! `disconnect` command stops and awaits it. Windows controllers can launch
//! the system RDP client after the library reports a bound local listener.

mod rdp_launch_state;
mod rdp_launcher;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use rdp_launch_state::RdpLaunchCoordinator;
use rdp_launcher::{LaunchedRdp, RdpLaunchRequest};
use serde::{Deserialize, Serialize};
use spuria_client::{app, forwarding::ForwardRule, AppConfig};
use spuria_common::{ids::DeviceId, transport::Role};
use tauri::{async_runtime::JoinHandle, AppHandle, Emitter, Manager, State};
use tokio::sync::{oneshot, Mutex};

/// Holds the currently-running session task (if any).
#[derive(Default)]
struct AppState {
    task: Mutex<Option<RunningClient>>,
    closing: AtomicBool,
    next_forward_id: AtomicU64,
}

struct RunningClient {
    stop: oneshot::Sender<()>,
    stopping: Arc<AtomicBool>,
    task: JoinHandle<()>,
    controls: app::AppControl,
}

impl RunningClient {
    async fn shutdown(self) {
        self.stopping.store(true, Ordering::SeqCst);
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
    rdp_launch_supported: bool,
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
    /// One connection only; never serialized into settings or events.
    #[serde(default)]
    rdp_launch: Option<RdpLaunchRequest>,
    /// Host only: TCP services on 127.0.0.1 that this run may expose.
    #[serde(default)]
    allow_forward_ports: Vec<u16>,
}

#[derive(Serialize)]
struct ForwardInfo {
    forward_id: String,
    listen_addr: String,
    remote_port: u16,
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
        rdp_launch_supported: rdp_launcher::available(),
    })
}

#[tauri::command]
async fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    mut opts: ConnectOpts,
) -> Result<String, String> {
    if state.closing.load(Ordering::SeqCst) {
        return Err("The application is closing.".into());
    }
    let settings = load_settings(&app)?;
    if settings.server.trim().is_empty() {
        return Err("No signaling server configured — open Settings and save first.".into());
    }
    let role = match opts.role.as_str() {
        "host" => Role::Host,
        "controller" => Role::Controller,
        _ => return Err("Unknown client role.".into()),
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
    app::validate_forward_ports(&opts.allow_forward_ports)?;
    if role != Role::Host && !opts.allow_forward_ports.is_empty() {
        return Err("Only a host can allow remote forwarding ports.".into());
    }

    // Include credential preparation in command serialization. Disconnect
    // must not return while an earlier Connect is still preparing a late run.
    let mut running = state.task.lock().await;
    if state.closing.load(Ordering::SeqCst) {
        return Err("The application is closing.".into());
    }
    let prepared_rdp = if let Some(request) = opts.rdp_launch.take() {
        if role != Role::Controller {
            return Err("Automatic Remote Desktop is only available in controller mode.".into());
        }
        if !listen_addr.ip().is_loopback() {
            return Err(
                "Automatic Remote Desktop requires a loopback listener, such as 127.0.0.1:33389."
                    .into(),
            );
        }
        Some(
            tokio::task::spawn_blocking(move || rdp_launcher::prepare(request))
                .await
                .map_err(|_| "Could not prepare Remote Desktop credentials.".to_string())??,
        )
    } else {
        None
    };
    let run_data_dir = data_dir(&app)?;
    let expected_peer = peer_id
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();

    // Event forwarding is owned by the same task as the client, so an old
    // connection cannot emit stale events after its replacement starts.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (controls, control_rx) = app::control_channel();

    let cfg = AppConfig {
        role,
        server_url: settings.server,
        reflect_addr,
        device_id: DeviceId::new(device_id.clone()),
        secret: settings.secret,
        peer_id,
        rdp_addr,
        listen_addr,
        data_dir: run_data_dir.clone(),
        force_relay: opts.force_relay,
        enable_udp: settings.enable_udp,
        events: Some(tx),
        allow_forward_ports: opts.allow_forward_ports,
        initial_forwards: Vec::new(),
        controls: Some(control_rx),
    };

    // Serialize replacement and disconnect through the whole shutdown. The
    // command returns only after previous RDP listeners/bridges have stopped.
    if state.closing.load(Ordering::SeqCst) {
        return Err("The application is closing.".into());
    }
    if let Some(previous) = running.take() {
        previous.shutdown().await;
    }
    let (stop, stopped) = oneshot::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let native_stop = stopping.clone();
    let app_for_run = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let launcher = prepared_rdp.map(|prepared| {
            Arc::new(move |address| prepared.launch(&run_data_dir, address))
                as Arc<dyn Fn(SocketAddr) -> Result<LaunchedRdp, String> + Send + Sync>
        });
        let mut native =
            RdpLaunchCoordinator::new(expected_peer, listen_addr, native_stop, launcher);
        let run = app::run_until_shutdown(cfg, async {
            let _ = stopped.await;
        });
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                biased;
                result = &mut run => break result,
                Some(ev) = rx.recv() => {
                    // Display the readiness event before reporting launch results.
                    let _ = app_for_run.emit("client-event", &ev);
                    if let Some(result) = native.on_event(&ev) {
                        let _ = app_for_run.emit("rdp-launch", result);
                    }
                }
                event = native.next_event() => { let _ = app_for_run.emit("rdp-launch", event); }
            }
        };
        native.shutdown().await;
        // The client has exited. Queued readiness events are display-only and
        // must never authorize another process launch during this final drain.
        while let Ok(ev) = rx.try_recv() {
            let _ = app_for_run.emit("client-event", ev);
        }
        if let Err(e) = result {
            let _ = app_for_run.emit("client-error", e.to_string());
        }
        let _ = app_for_run.emit("client-stopped", ());
    });
    *running = Some(RunningClient {
        stop,
        stopping,
        task: handle,
        controls,
    });

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

/// Clone the handle for this particular client run. Releasing the task lock
/// before waiting lets Disconnect cancel a pending command and drain sockets.
async fn forwarding_control(state: &AppState) -> Result<app::AppControl, String> {
    let running = state.task.lock().await;
    if state.closing.load(Ordering::SeqCst) {
        return Err("The application is closing.".into());
    }
    let client = running
        .as_ref()
        .filter(|client| !client.stopping.load(Ordering::SeqCst))
        .ok_or_else(|| "Connect to a remote device before forwarding a port.".to_string())?;
    Ok(client.controls.clone())
}

#[tauri::command]
async fn start_port_forward(
    state: State<'_, AppState>,
    session_id: String,
    listen_addr: String,
    remote_port: u16,
) -> Result<ForwardInfo, String> {
    let listen_addr: SocketAddr = listen_addr
        .trim()
        .parse()
        .map_err(|_| "Enter a local loopback address, such as 127.0.0.1:8080.".to_string())?;
    if !listen_addr.ip().is_loopback() || listen_addr.port() == 0 || remote_port == 0 {
        return Err("Use a loopback listener and ports between 1 and 65535.".into());
    }
    let controls = forwarding_control(&state).await?;
    let forward_id = format!(
        "forward-{}",
        state.next_forward_id.fetch_add(1, Ordering::Relaxed)
    );
    let bound = controls
        .start(
            session_id,
            ForwardRule {
                id: forward_id.clone(),
                listen_addr,
                remote_port,
            },
        )
        .await?;
    Ok(ForwardInfo {
        forward_id,
        listen_addr: bound.to_string(),
        remote_port,
    })
}

#[tauri::command]
async fn stop_port_forward(
    state: State<'_, AppState>,
    session_id: String,
    forward_id: String,
) -> Result<(), String> {
    forwarding_control(&state)
        .await?
        .stop(session_id, forward_id)
        .await
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
            start_port_forward,
            stop_port_forward,
            check_update
        ])
        .build(tauri::generate_context!())
        .expect("error while building Spuria desktop")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested {
                code: None, api, ..
            } = event
            {
                api.prevent_exit();
                let state = app.state::<AppState>();
                if !state.closing.swap(true, Ordering::SeqCst) {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let state = app.state::<AppState>();
                        let mut running = state.task.lock().await;
                        if let Some(client) = running.take() {
                            client.shutdown().await;
                        }
                        app.exit(0);
                    });
                }
            }
        });
}
